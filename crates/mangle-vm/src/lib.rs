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
pub use mangle_common::{Host, HostVal};
use wasmtime::{Engine, ExternRef, Linker, Module, Rooted, Store};

#[cfg(feature = "csv_storage")]
pub mod csv_host;

pub mod composite_host;

pub struct Vm {
    engine: Engine,
}

struct HostWrapper<H> {
    host: H,
    strings: Vec<String>,
    names: Vec<String>,
}

/// Extract a HostVal from an Option<Rooted<ExternRef>>.
fn extract_hv<T>(val: &Option<Rooted<ExternRef>>, caller: &wasmtime::Caller<'_, T>) -> HostVal {
    let n = val
        .as_ref()
        .and_then(|r| r.data(caller).ok())
        .flatten()
        .and_then(|d| d.downcast_ref::<u32>().copied())
        .unwrap_or(0);
    HostVal(n)
}

/// Create an ExternRef from a HostVal.
fn make_ref<H>(
    caller: &mut wasmtime::Caller<'_, HostWrapper<H>>,
    hv: HostVal,
) -> Result<Option<Rooted<ExternRef>>> {
    let r = ExternRef::new(caller, hv.0)?;
    Ok(Some(r))
}

impl Vm {
    pub fn new() -> Result<Self> {
        let engine = Engine::default();
        Ok(Self { engine })
    }

    pub fn execute<H: Host + Send + Sync + 'static>(
        &self,
        wasm: &[u8],
        host: H,
        strings: Vec<String>,
        names: Vec<String>,
    ) -> Result<()> {
        let module = Module::new(&self.engine, wasm)?;
        let mut store = Store::new(
            &self.engine,
            HostWrapper {
                host,
                strings,
                names,
            },
        );

        let mut linker = Linker::new(&self.engine);

        // 0: scan_start(i32) -> i32
        linker.func_wrap(
            "env",
            "scan_start",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>, rel_id: i32| -> i32 {
                caller.data_mut().host.scan_start(rel_id)
            },
        )?;

        // 1: scan_next(i32) -> i32
        linker.func_wrap(
            "env",
            "scan_next",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>, iter_id: i32| -> i32 {
                caller.data_mut().host.scan_next(iter_id)
            },
        )?;

        // 2: get_col(i32, i32) -> externref
        linker.func_wrap(
            "env",
            "get_col",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             ptr: i32,
             idx: i32|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.get_col(ptr, idx);
                make_ref(&mut caller, hv)
            },
        )?;

        // 3: insert_begin(i32) -> ()
        linker.func_wrap(
            "env",
            "insert_begin",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>, rel_id: i32| {
                caller.data_mut().host.insert_begin(rel_id);
            },
        )?;

        // 4: insert_push(externref) -> ()
        linker.func_wrap(
            "env",
            "insert_push",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             val: Option<Rooted<ExternRef>>| {
                let hv = extract_hv(&val, &caller);
                caller.data_mut().host.insert_push(hv);
            },
        )?;

        // 5: insert_end() -> ()
        linker.func_wrap(
            "env",
            "insert_end",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>| {
                caller.data_mut().host.insert_end();
            },
        )?;

        // 6: scan_delta_start(i32) -> i32
        linker.func_wrap(
            "env",
            "scan_delta_start",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>, rel_id: i32| -> i32 {
                caller.data_mut().host.scan_delta_start(rel_id)
            },
        )?;

        // 7: merge_deltas() -> i32
        linker.func_wrap(
            "env",
            "merge_deltas",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>| -> i32 {
                caller.data_mut().host.merge_deltas()
            },
        )?;

        // 8: debuglog(externref) -> ()
        linker.func_wrap(
            "env",
            "debuglog",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             val: Option<Rooted<ExternRef>>| {
                let hv = extract_hv(&val, &caller);
                caller.data_mut().host.debuglog(hv);
            },
        )?;

        // 9: scan_index_start(i32, i32, externref) -> i32
        linker.func_wrap(
            "env",
            "scan_index_start",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             rel_id: i32,
             col_idx: i32,
             val: Option<Rooted<ExternRef>>|
             -> i32 {
                let hv = extract_hv(&val, &caller);
                caller
                    .data_mut()
                    .host
                    .scan_index_start(rel_id, col_idx, hv)
            },
        )?;

        // 10: scan_aggregate_start(i32, i32, i32) -> i32
        linker.func_wrap(
            "env",
            "scan_aggregate_start",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             rel_id: i32,
             ptr: i32,
             len: i32|
             -> i32 {
                let mem = caller
                    .get_export("memory")
                    .expect("memory export not found")
                    .into_memory()
                    .expect("not a memory");
                let data = mem.data(&caller);
                let start = ptr as usize;
                let end = start + (len as usize) * 4;
                let bytes = &data[start..end];
                let mut desc = Vec::with_capacity(len as usize);
                for chunk in bytes.chunks_exact(4) {
                    desc.push(i32::from_le_bytes(chunk.try_into().unwrap()));
                }
                caller
                    .data_mut()
                    .host
                    .scan_aggregate_start(rel_id, desc)
            },
        )?;

        // 11: const_number(i64) -> externref
        linker.func_wrap(
            "env",
            "const_number",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             n: i64|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_number(n);
                make_ref(&mut caller, hv)
            },
        )?;

        // 12: const_float(i64) -> externref
        linker.func_wrap(
            "env",
            "const_float",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             bits: i64|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_float(bits);
                make_ref(&mut caller, hv)
            },
        )?;

        // 13: const_string(i32) -> externref
        linker.func_wrap(
            "env",
            "const_string",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             id: i32|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_string(id);
                make_ref(&mut caller, hv)
            },
        )?;

        // 14: const_name(i32) -> externref
        linker.func_wrap(
            "env",
            "const_name",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             id: i32|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_name(id);
                make_ref(&mut caller, hv)
            },
        )?;

        // 15: const_time(i64) -> externref
        linker.func_wrap(
            "env",
            "const_time",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             nanos: i64|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_time(nanos);
                make_ref(&mut caller, hv)
            },
        )?;

        // 16: const_duration(i64) -> externref
        linker.func_wrap(
            "env",
            "const_duration",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             nanos: i64|
             -> Result<Option<Rooted<ExternRef>>> {
                let hv = caller.data_mut().host.const_duration(nanos);
                make_ref(&mut caller, hv)
            },
        )?;

        // 17-20: val_add/sub/mul/div (externref, externref) -> externref
        macro_rules! binop {
            ($name:expr, $method:ident) => {
                linker.func_wrap(
                    "env",
                    $name,
                    |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
                     a: Option<Rooted<ExternRef>>,
                     b: Option<Rooted<ExternRef>>|
                     -> Result<Option<Rooted<ExternRef>>> {
                        let a_hv = extract_hv(&a, &caller);
                        let b_hv = extract_hv(&b, &caller);
                        let result = caller.data_mut().host.$method(a_hv, b_hv);
                        make_ref(&mut caller, result)
                    },
                )?;
            };
        }
        binop!("val_add", val_add);
        binop!("val_sub", val_sub);
        binop!("val_mul", val_mul);
        binop!("val_div", val_div);

        // 21: val_sqrt(externref) -> externref
        linker.func_wrap(
            "env",
            "val_sqrt",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             a: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let a_hv = extract_hv(&a, &caller);
                let result = caller.data_mut().host.val_sqrt(a_hv);
                make_ref(&mut caller, result)
            },
        )?;

        // 22-27: val_eq/neq/lt/le/gt/ge (externref, externref) -> i32
        macro_rules! cmpop {
            ($name:expr, $method:ident) => {
                linker.func_wrap(
                    "env",
                    $name,
                    |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
                     a: Option<Rooted<ExternRef>>,
                     b: Option<Rooted<ExternRef>>|
                     -> i32 {
                        let a_hv = extract_hv(&a, &caller);
                        let b_hv = extract_hv(&b, &caller);
                        caller.data_mut().host.$method(a_hv, b_hv)
                    },
                )?;
            };
        }
        cmpop!("val_eq", val_eq);
        cmpop!("val_neq", val_neq);
        cmpop!("val_lt", val_lt);
        cmpop!("val_le", val_le);
        cmpop!("val_gt", val_gt);
        cmpop!("val_ge", val_ge);

        // 28: str_concat (externref, externref) -> externref
        binop!("str_concat", str_concat);

        // 29: str_replace (externref, externref, externref, externref) -> externref
        linker.func_wrap(
            "env",
            "str_replace",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             s: Option<Rooted<ExternRef>>,
             old: Option<Rooted<ExternRef>>,
             new: Option<Rooted<ExternRef>>,
             count: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let s_hv = extract_hv(&s, &caller);
                let old_hv = extract_hv(&old, &caller);
                let new_hv = extract_hv(&new, &caller);
                let count_hv = extract_hv(&count, &caller);
                let result = caller.data_mut().host.str_replace(s_hv, old_hv, new_hv, count_hv);
                make_ref(&mut caller, result)
            },
        )?;

        // 30: val_to_string (externref) -> externref
        linker.func_wrap(
            "env",
            "val_to_string",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             a: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let a_hv = extract_hv(&a, &caller);
                let result = caller.data_mut().host.val_to_string(a_hv);
                make_ref(&mut caller, result)
            },
        )?;

        // 31: compound_begin (i32) -> ()
        linker.func_wrap(
            "env",
            "compound_begin",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>, kind: i32| {
                caller.data_mut().host.compound_begin(kind);
            },
        )?;

        // 32: compound_push (externref) -> ()
        linker.func_wrap(
            "env",
            "compound_push",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             val: Option<Rooted<ExternRef>>| {
                let hv = extract_hv(&val, &caller);
                caller.data_mut().host.compound_push(hv);
            },
        )?;

        // 33: compound_end () -> externref
        linker.func_wrap(
            "env",
            "compound_end",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let result = caller.data_mut().host.compound_end();
                make_ref(&mut caller, result)
            },
        )?;

        // 34: compound_get (externref, externref) -> externref
        binop!("compound_get", compound_get);

        // 35: compound_len (externref) -> externref
        linker.func_wrap(
            "env",
            "compound_len",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             a: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let a_hv = extract_hv(&a, &caller);
                let result = caller.data_mut().host.compound_len(a_hv);
                make_ref(&mut caller, result)
            },
        )?;

        // 36: pair_first (externref) -> externref
        linker.func_wrap(
            "env",
            "pair_first",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             a: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let a_hv = extract_hv(&a, &caller);
                let result = caller.data_mut().host.pair_first(a_hv);
                make_ref(&mut caller, result)
            },
        )?;

        // 37: pair_second (externref) -> externref
        linker.func_wrap(
            "env",
            "pair_second",
            |mut caller: wasmtime::Caller<'_, HostWrapper<H>>,
             a: Option<Rooted<ExternRef>>|
             -> Result<Option<Rooted<ExternRef>>> {
                let a_hv = extract_hv(&a, &caller);
                let result = caller.data_mut().host.pair_second(a_hv);
                make_ref(&mut caller, result)
            },
        )?;

        let instance = linker.instantiate(&mut store, &module)?;
        let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
        run.call(&mut store, ())?;

        Ok(())
    }
}

// Minimal dummy host for tests that don't need storage
pub struct DummyHost;
impl Host for DummyHost {
    fn scan_start(&mut self, _rel_id: i32) -> i32 { 0 }
    fn scan_delta_start(&mut self, _rel_id: i32) -> i32 { 0 }
    fn scan_next(&mut self, _iter_id: i32) -> i32 { 0 }
    fn merge_deltas(&mut self) -> i32 { 0 }
    fn scan_aggregate_start(&mut self, _rel_id: i32, _desc: Vec<i32>) -> i32 { 0 }
    fn scan_index_start(&mut self, _rel_id: i32, _col_idx: i32, _val: HostVal) -> i32 { 0 }
    fn get_col(&mut self, _ptr: i32, _idx: i32) -> HostVal { HostVal(0) }
    fn insert_begin(&mut self, _rel_id: i32) {}
    fn insert_push(&mut self, _val: HostVal) {}
    fn insert_end(&mut self) {}
    fn const_number(&mut self, _n: i64) -> HostVal { HostVal(0) }
    fn const_float(&mut self, _bits: i64) -> HostVal { HostVal(0) }
    fn const_string(&mut self, _id: i32) -> HostVal { HostVal(0) }
    fn const_name(&mut self, _id: i32) -> HostVal { HostVal(0) }
    fn const_time(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
    fn const_duration(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
    fn val_add(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn val_sub(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn val_mul(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn val_div(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
    fn val_sqrt(&mut self, _a: HostVal) -> HostVal { HostVal(0) }
    fn val_eq(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
    fn val_neq(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
    fn val_lt(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
    fn val_le(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
    fn val_gt(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
    fn val_ge(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
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

#[cfg(test)]
mod tests {
    use super::*;
    use fxhash::FxHashSet;
    use mangle_analysis::{rewrite_unit, LoweringContext, Program};
    use mangle_ast as ast;
    use mangle_codegen::{Codegen, WasmImportsBackend};
    use mangle_parse::Parser;
    use std::collections::HashMap;

    #[test]
    fn test_e2e_execution() -> Result<()> {
        let arena = ast::Arena::new_with_global_interner();
        let foo = arena.predicate_sym("foo", Some(1));
        let bar = arena.predicate_sym("bar", Some(1));
        let x = arena.variable("X");

        let clause = ast::Clause {
            head: arena.atom(foo, &[x]),
            head_time: None,
            premises: arena
                .alloc_slice_copy(&[arena.alloc(ast::Term::Atom(arena.atom(bar, &[x])))]),
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

        let vm = Vm::new()?;
        vm.execute(&compiled.wasm, DummyHost, compiled.strings, compiled.names)?;

        Ok(())
    }

    #[test]
    fn test_e2e_function() -> Result<()> {
        let arena = ast::Arena::new_with_global_interner();
        let foo = arena.predicate_sym("foo", Some(1));
        let plus = arena.function_sym("fn:plus", Some(2));

        let c1 = arena.const_(ast::Const::Number(1));
        let c2 = arena.const_(ast::Const::Number(2));

        let head_arg = arena.apply_fn(plus, &[c1, c2]);
        let clause = ast::Clause {
            head: arena.atom(foo, &[head_arg]),
            head_time: None,
            premises: &[],
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

        let vm = Vm::new()?;
        vm.execute(&compiled.wasm, DummyHost, compiled.strings, compiled.names)?;
        Ok(())
    }

    // --- Value-aware MemHost ---

    #[derive(Debug, Clone, PartialEq)]
    enum Val {
        Number(i64),
        Float(f64),
        String(String),
        Time(i64),
        Duration(i64),
        /// Compound: (kind, elements). kind: 0=List, 1=Pair, 2=Map, 3=Struct.
        Compound(i32, Vec<HostVal>),
    }

    struct MemHost {
        /// Value slab: HostVal(n) indexes into values[n]
        values: Vec<Val>,
        /// rel_id -> tuples of HostVal handles
        data: HashMap<i32, Vec<Vec<HostVal>>>,
        iters: HashMap<i32, (i32, usize)>,
        next_iter_id: i32,
        /// Pending multi-column insert
        pending_rel: i32,
        pending_tuple: Vec<HostVal>,
        /// Pending compound build
        compound_kind: i32,
        compound_elems: Vec<HostVal>,
        /// String/name tables from compiled module
        strings: Vec<String>,
        names: Vec<String>,
    }

    impl MemHost {
        fn new(strings: Vec<String>, names: Vec<String>) -> Self {
            Self {
                values: Vec::new(),
                data: HashMap::new(),
                iters: HashMap::new(),
                next_iter_id: 1,
                pending_rel: 0,
                pending_tuple: Vec::new(),
                compound_kind: 0,
                compound_elems: Vec::new(),
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

        fn val_to_str(&self, hv: HostVal) -> String {
            match self.get_val(hv) {
                Val::Number(n) => n.to_string(),
                Val::Float(f) => f.to_string(),
                Val::String(s) => s.clone(),
                Val::Time(t) => format!("time({})", t),
                Val::Duration(d) => format!("duration({})", d),
                Val::Compound(kind, _) => format!("compound(kind={})", kind),
            }
        }

        fn add_number_fact(&mut self, rel: &str, args: &[i64]) {
            let id = Self::hash_name(rel);
            let hvs: Vec<HostVal> = args.iter().map(|n| self.alloc(Val::Number(*n))).collect();
            self.data.entry(id).or_default().push(hvs);
        }

        fn add_string_fact(&mut self, rel: &str, args: &[&str]) {
            let id = Self::hash_name(rel);
            let hvs: Vec<HostVal> = args.iter().map(|s| self.alloc(Val::String(s.to_string()))).collect();
            self.data.entry(id).or_default().push(hvs);
        }

        fn get_number_facts(&self, rel: &str) -> Vec<Vec<i64>> {
            let id = Self::hash_name(rel);
            self.data
                .get(&id)
                .map(|tuples| {
                    tuples
                        .iter()
                        .map(|t| {
                            t.iter()
                                .map(|hv| match self.get_val(*hv) {
                                    Val::Number(n) => *n,
                                    _ => 0,
                                })
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        fn get_string_facts(&self, rel: &str) -> Vec<Vec<String>> {
            let id = Self::hash_name(rel);
            self.data
                .get(&id)
                .map(|tuples| {
                    tuples
                        .iter()
                        .map(|t| {
                            t.iter()
                                .map(|hv| match self.get_val(*hv) {
                                    Val::String(s) => s.clone(),
                                    other => format!("{:?}", other),
                                })
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        fn get_val_facts(&self, rel: &str) -> Vec<Vec<Val>> {
            let id = Self::hash_name(rel);
            self.data
                .get(&id)
                .map(|tuples| {
                    tuples
                        .iter()
                        .map(|t| t.iter().map(|hv| self.get_val(*hv).clone()).collect())
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    impl Host for MemHost {
        fn scan_start(&mut self, rel_id: i32) -> i32 {
            let id = self.next_iter_id;
            self.next_iter_id += 1;
            self.iters.insert(id, (rel_id, 0));
            id
        }
        fn scan_delta_start(&mut self, rel_id: i32) -> i32 {
            self.scan_start(rel_id)
        }
        fn scan_next(&mut self, iter_id: i32) -> i32 {
            if let Some((rel_id, idx)) = self.iters.get_mut(&iter_id)
                && let Some(tuples) = self.data.get(rel_id)
                && *idx < tuples.len()
            {
                let ptr = (iter_id << 16) | (*idx as i32 + 1);
                *idx += 1;
                return ptr;
            }
            0
        }
        fn merge_deltas(&mut self) -> i32 { 0 }
        fn scan_aggregate_start(&mut self, _rel_id: i32, _desc: Vec<i32>) -> i32 { 0 }
        fn scan_index_start(&mut self, _rel_id: i32, _col_idx: i32, _val: HostVal) -> i32 { 0 }

        fn get_col(&mut self, ptr: i32, col_idx: i32) -> HostVal {
            let iter_id = ptr >> 16;
            let tuple_idx = (ptr & 0xFFFF) - 1;
            if let Some((rel_id, _)) = self.iters.get(&iter_id)
                && let Some(tuples) = self.data.get(rel_id)
            {
                return tuples[tuple_idx as usize][col_idx as usize];
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
            self.data.entry(self.pending_rel).or_default().push(tuple);
        }

        fn const_number(&mut self, n: i64) -> HostVal { self.alloc(Val::Number(n)) }
        fn const_float(&mut self, bits: i64) -> HostVal { self.alloc(Val::Float(f64::from_bits(bits as u64))) }
        fn const_string(&mut self, id: i32) -> HostVal {
            let s = self.strings.get((id - 1) as usize).cloned().unwrap_or_default();
            self.alloc(Val::String(s))
        }
        fn const_name(&mut self, id: i32) -> HostVal {
            let s = self.names.get((id - 1) as usize).cloned().unwrap_or_default();
            self.alloc(Val::String(s))
        }
        fn const_time(&mut self, nanos: i64) -> HostVal { self.alloc(Val::Time(nanos)) }
        fn const_duration(&mut self, nanos: i64) -> HostVal { self.alloc(Val::Duration(nanos)) }

        fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal {
            let result = match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => Val::Number(a + b),
                (Val::Float(a), Val::Float(b)) => Val::Float(a + b),
                (Val::Number(a), Val::Float(b)) => Val::Float(*a as f64 + b),
                (Val::Float(a), Val::Number(b)) => Val::Float(a + *b as f64),
                _ => Val::Number(0),
            };
            self.alloc(result)
        }
        fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal {
            let result = match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => Val::Number(a - b),
                (Val::Float(a), Val::Float(b)) => Val::Float(a - b),
                (Val::Number(a), Val::Float(b)) => Val::Float(*a as f64 - b),
                (Val::Float(a), Val::Number(b)) => Val::Float(a - *b as f64),
                _ => Val::Number(0),
            };
            self.alloc(result)
        }
        fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal {
            let result = match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => Val::Number(a * b),
                (Val::Float(a), Val::Float(b)) => Val::Float(a * b),
                (Val::Number(a), Val::Float(b)) => Val::Float(*a as f64 * b),
                (Val::Float(a), Val::Number(b)) => Val::Float(a * *b as f64),
                _ => Val::Number(0),
            };
            self.alloc(result)
        }
        fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal {
            let result = match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) if *b != 0 => Val::Number(a / b),
                (Val::Float(a), Val::Float(b)) => Val::Float(a / b),
                (Val::Number(a), Val::Float(b)) => Val::Float(*a as f64 / b),
                (Val::Float(a), Val::Number(b)) => Val::Float(a / *b as f64),
                _ => Val::Number(0),
            };
            self.alloc(result)
        }
        fn val_sqrt(&mut self, a: HostVal) -> HostVal {
            let result = match self.get_val(a) {
                Val::Float(f) => Val::Float(f.sqrt()),
                Val::Number(n) => Val::Float((*n as f64).sqrt()),
                _ => Val::Float(0.0),
            };
            self.alloc(result)
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
                (Val::Float(a), Val::Float(b)) => (a < b) as i32,
                _ => 0,
            }
        }
        fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 {
            match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => (a <= b) as i32,
                (Val::Float(a), Val::Float(b)) => (a <= b) as i32,
                _ => 0,
            }
        }
        fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 {
            match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => (a > b) as i32,
                (Val::Float(a), Val::Float(b)) => (a > b) as i32,
                _ => 0,
            }
        }
        fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 {
            match (self.get_val(a), self.get_val(b)) {
                (Val::Number(a), Val::Number(b)) => (a >= b) as i32,
                (Val::Float(a), Val::Float(b)) => (a >= b) as i32,
                _ => 0,
            }
        }
        fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal {
            let sa = self.val_to_str(a);
            let sb = self.val_to_str(b);
            self.alloc(Val::String(format!("{}{}", sa, sb)))
        }
        fn str_replace(&mut self, s: HostVal, old: HostVal, new: HostVal, count: HostVal) -> HostVal {
            let s_str = self.val_to_str(s);
            let old_str = self.val_to_str(old);
            let new_str = self.val_to_str(new);
            let count_val = match self.get_val(count) {
                Val::Number(n) => *n,
                _ => -1,
            };
            let result = if count_val < 0 {
                s_str.replace(&old_str, &new_str)
            } else {
                s_str.replacen(&old_str, &new_str, count_val as usize)
            };
            self.alloc(Val::String(result))
        }
        fn val_to_string(&mut self, val: HostVal) -> HostVal {
            let s = self.val_to_str(val);
            self.alloc(Val::String(s))
        }
        fn compound_begin(&mut self, kind: i32) {
            self.compound_kind = kind;
            self.compound_elems.clear();
        }
        fn compound_push(&mut self, val: HostVal) {
            self.compound_elems.push(val);
        }
        fn compound_end(&mut self) -> HostVal {
            let elems = std::mem::take(&mut self.compound_elems);
            self.alloc(Val::Compound(self.compound_kind, elems))
        }
        fn compound_get(&mut self, compound: HostVal, key: HostVal) -> HostVal {
            if let Val::Compound(kind, elems) = self.get_val(compound).clone() {
                match kind {
                    0 => {
                        // List: key is index (Number)
                        if let Val::Number(idx) = self.get_val(key) {
                            return elems.get(*idx as usize).copied().unwrap_or(HostVal(0));
                        }
                    }
                    2 | 3 => {
                        // Map/Struct: even positions are keys, odd are values
                        for i in (0..elems.len()).step_by(2) {
                            if i + 1 < elems.len() && self.get_val(elems[i]) == self.get_val(key) {
                                return elems[i + 1];
                            }
                        }
                    }
                    _ => {}
                }
            }
            HostVal(0)
        }
        fn compound_len(&mut self, compound: HostVal) -> HostVal {
            if let Val::Compound(kind, elems) = self.get_val(compound).clone() {
                let len = match kind {
                    0 | 1 => elems.len() as i64,       // List/Pair: element count
                    2 | 3 => (elems.len() / 2) as i64, // Map/Struct: key-value pair count
                    _ => 0,
                };
                return self.alloc(Val::Number(len));
            }
            self.alloc(Val::Number(0))
        }
        fn pair_first(&mut self, compound: HostVal) -> HostVal {
            if let Val::Compound(_, elems) = self.get_val(compound).clone() {
                return elems.first().copied().unwrap_or(HostVal(0));
            }
            HostVal(0)
        }
        fn pair_second(&mut self, compound: HostVal) -> HostVal {
            if let Val::Compound(_, elems) = self.get_val(compound).clone() {
                return elems.get(1).copied().unwrap_or(HostVal(0));
            }
            HostVal(0)
        }
        fn debuglog(&mut self, val: HostVal) {
            eprintln!("WASM LOG: {:?}", self.get_val(val));
        }
    }

    // --- SharedMemHost wrapper for thread-safety ---

    use std::sync::{Arc, Mutex};

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

    /// Helper: run MemHost through WASM with pre-loaded data.
    fn run_host_wasm(compiled: &mangle_codegen::CompiledModule, host: MemHost) -> Result<MemHost> {
        let shared_host = SharedMemHost {
            inner: Arc::new(Mutex::new(host)),
        };
        let vm = Vm::new()?;
        vm.execute(
            &compiled.wasm,
            shared_host.clone(),
            compiled.strings.clone(),
            compiled.names.clone(),
        )?;
        let host = Arc::try_unwrap(shared_host.inner)
            .map_err(|_| anyhow::anyhow!("Arc still shared"))?
            .into_inner()
            .unwrap();
        Ok(host)
    }

    /// Helper: compile Mangle source to WASM and execute with a MemHost.
    fn run_wasm_program(source: &str) -> Result<MemHost> {
        let arena = ast::Arena::new_with_global_interner();
        let mut parser = Parser::new(&arena, source.as_bytes(), arena.alloc_str("test"));
        parser.next_token().map_err(|e| anyhow::anyhow!(e))?;
        let unit = parser.parse_unit()?;
        let unit = rewrite_unit(&arena, &unit);

        let mut program = Program::new(&arena);
        let mut all_preds = FxHashSet::default();
        let mut idb_preds = FxHashSet::default();
        for clause in unit.clauses {
            program.add_clause(&arena, clause);
            idb_preds.insert(clause.head.sym);
            all_preds.insert(clause.head.sym);
            for premise in clause.premises {
                match premise {
                    ast::Term::Atom(atom) => { all_preds.insert(atom.sym); }
                    ast::Term::NegAtom(atom) => { all_preds.insert(atom.sym); }
                    ast::Term::TemporalAtom(atom, _) => { all_preds.insert(atom.sym); }
                    _ => {}
                }
            }
        }
        for pred in all_preds {
            if !idb_preds.contains(&pred) {
                program.ext_preds.push(pred);
            }
        }
        let stratified = program.stratify().map_err(|e| anyhow::anyhow!(e))?;

        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);

        let mut codegen = Codegen::new_with_stratified(&mut ir, &stratified, WasmImportsBackend);
        let compiled = codegen.generate();
        let host = MemHost::new(compiled.strings.clone(), compiled.names.clone());
        run_host_wasm(&compiled, host)
    }

    #[test]
    fn test_e2e_mem_store() -> Result<()> {
        let arena = ast::Arena::new_with_global_interner();
        let p = arena.predicate_sym("p", Some(1));
        let q = arena.predicate_sym("q", Some(1));
        let x = arena.variable("X");

        let clause = ast::Clause {
            head: arena.atom(p, &[x]),
            head_time: None,
            premises: arena.alloc_slice_copy(&[arena.alloc(ast::Term::Atom(arena.atom(q, &[x])))]),
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

        let mut host = MemHost::new(compiled.strings.clone(), compiled.names.clone());
        host.add_number_fact("q", &[10]);
        host.add_number_fact("q", &[20]);

        let host = run_host_wasm(&compiled, host)?;
        let results = host.get_number_facts("p");

        assert!(results.iter().any(|t| t[0] == 10), "expected 10 in results: {:?}", results);
        assert!(results.iter().any(|t| t[0] == 20), "expected 20 in results: {:?}", results);

        Ok(())
    }

    // ===== String E2E Tests =====

    #[test]
    fn test_wasm_string_constant() -> Result<()> {
        let host = run_wasm_program(r#"
            p("hello").
            q(X) :- p(X).
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "hello");
        Ok(())
    }

    #[test]
    fn test_wasm_string_equality() -> Result<()> {
        let host = run_wasm_program(r#"
            p("hello"). p("world").
            q(X) :- p(X), X = "hello".
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "hello");
        Ok(())
    }

    #[test]
    fn test_wasm_string_concat() -> Result<()> {
        let host = run_wasm_program(r#"
            p("hello", "world").
            q(R) :- p(A, B) |> let R = fn:string:concat(A, " ", B).
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "hello world");
        Ok(())
    }

    #[test]
    fn test_wasm_string_replace() -> Result<()> {
        let host = run_wasm_program(r#"
            p("foo-bar-baz").
            q(R) :- p(S) |> let R = fn:string:replace(S, "-", "_", -1).
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "foo_bar_baz");
        Ok(())
    }

    #[test]
    fn test_wasm_number_to_string() -> Result<()> {
        let host = run_wasm_program(r#"
            p(42).
            q(R) :- p(X) |> let R = fn:number:to_string(X).
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "42");
        Ok(())
    }

    // ===== Compound E2E Tests =====

    #[test]
    fn test_wasm_list_construction() -> Result<()> {
        let host = run_wasm_program(r#"
            p(1, 2, 3).
            q(L) :- p(A, B, C) |> let L = fn:list(A, B, C).
        "#)?;
        let results = host.get_val_facts("q");
        assert_eq!(results.len(), 1);
        if let Val::Compound(kind, elems) = &results[0][0] {
            assert_eq!(*kind, 0); // List
            assert_eq!(elems.len(), 3);
        } else {
            panic!("Expected compound, got {:?}", results[0][0]);
        }
        Ok(())
    }

    #[test]
    fn test_wasm_pair_construction_and_access() -> Result<()> {
        let host = run_wasm_program(r#"
            p(10, 20).
            mid(P) :- p(A, B) |> let P = fn:pair(A, B).
            q(F) :- mid(P) |> let F = fn:pair:first(P).
            r(S) :- mid(P) |> let S = fn:pair:second(P).
        "#)?;
        let q_results = host.get_number_facts("q");
        let r_results = host.get_number_facts("r");
        assert_eq!(q_results.len(), 1);
        assert_eq!(q_results[0], vec![10]);
        assert_eq!(r_results.len(), 1);
        assert_eq!(r_results[0], vec![20]);
        Ok(())
    }

    #[test]
    fn test_wasm_list_construction_and_len() -> Result<()> {
        let host = run_wasm_program(r#"
            p(10, 20, 30).
            mid(L) :- p(A, B, C) |> let L = fn:list(A, B, C).
            q(N) :- mid(L) |> let N = fn:len(L).
        "#)?;
        let results = host.get_number_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], vec![3]);
        Ok(())
    }

    #[test]
    fn test_wasm_list_get() -> Result<()> {
        let host = run_wasm_program(r#"
            p(10, 20, 30).
            mid(L) :- p(A, B, C) |> let L = fn:list(A, B, C).
            q(E) :- mid(L) |> let E = fn:list:get(L, 1).
        "#)?;
        let results = host.get_number_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], vec![20]);
        Ok(())
    }

    #[test]
    fn test_wasm_struct_construction_and_get() -> Result<()> {
        let host = run_wasm_program(r#"
            p("alice", 30).
            mid(S) :- p(Name, Age) |> let S = fn:struct(/name, Name, /age, Age).
            q(V) :- mid(S) |> let V = fn:struct:get(S, /name).
        "#)?;
        let results = host.get_string_facts("q");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][0], "alice");
        Ok(())
    }
}
