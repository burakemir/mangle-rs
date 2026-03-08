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

//! WASM code generation for Mangle programs.
//!
//! All Mangle values (numbers, floats, strings, compounds) are represented as
//! `externref` in the generated WASM. The host environment handles all value
//! operations (arithmetic, comparisons, constant creation) through imported
//! functions.

use std::collections::HashMap;

use mangle_analysis::{Planner, StratifiedProgram};
use mangle_ir::physical::{CmpOp, Condition, Constant, DataSource, Expr, Op, Operand};
use mangle_ir::{Inst, InstId, Ir, NameId};
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, HeapType,
    ImportSection, Instruction, MemorySection, Module, TypeSection, ValType,
};

// --- Import indices ---
// All host functions imported by the generated WASM module.
const IMP_SCAN_START: u32 = 0; //  (i32) -> i32
const IMP_SCAN_NEXT: u32 = 1; //  (i32) -> i32
const IMP_GET_COL: u32 = 2; //  (i32, i32) -> externref
const IMP_INSERT_BEGIN: u32 = 3; //  (i32) -> ()
const IMP_INSERT_PUSH: u32 = 4; //  (externref) -> ()
const IMP_INSERT_END: u32 = 5; //  () -> ()
const IMP_SCAN_DELTA_START: u32 = 6; //  (i32) -> i32
const IMP_MERGE_DELTAS: u32 = 7; //  () -> i32
const IMP_DEBUGLOG: u32 = 8; //  (externref) -> ()
const IMP_SCAN_INDEX_START: u32 = 9; //  (i32, i32, externref) -> i32
const IMP_SCAN_AGG_START: u32 = 10; // (i32, i32, i32) -> i32
const IMP_CONST_NUMBER: u32 = 11; // (i64) -> externref
const IMP_CONST_FLOAT: u32 = 12; // (i64) -> externref
const IMP_CONST_STRING: u32 = 13; // (i32) -> externref
const IMP_CONST_NAME: u32 = 14; // (i32) -> externref
const IMP_CONST_TIME: u32 = 15; // (i64) -> externref
const IMP_CONST_DURATION: u32 = 16; // (i64) -> externref
const IMP_VAL_ADD: u32 = 17; // (externref, externref) -> externref
const IMP_VAL_SUB: u32 = 18; // (externref, externref) -> externref
const IMP_VAL_MUL: u32 = 19; // (externref, externref) -> externref
const IMP_VAL_DIV: u32 = 20; // (externref, externref) -> externref
const IMP_VAL_SQRT: u32 = 21; // (externref) -> externref
const IMP_VAL_EQ: u32 = 22; // (externref, externref) -> i32
const IMP_VAL_NEQ: u32 = 23; // (externref, externref) -> i32
const IMP_VAL_LT: u32 = 24; // (externref, externref) -> i32
const IMP_VAL_LE: u32 = 25; // (externref, externref) -> i32
const IMP_VAL_GT: u32 = 26; // (externref, externref) -> i32
const IMP_VAL_GE: u32 = 27; // (externref, externref) -> i32
const IMP_STR_CONCAT: u32 = 28; // (externref, externref) -> externref
const IMP_STR_REPLACE: u32 = 29; // (externref, externref, externref, externref) -> externref
const IMP_VAL_TO_STRING: u32 = 30; // (externref) -> externref
const IMP_COMPOUND_BEGIN: u32 = 31; // (i32) -> ()
const IMP_COMPOUND_PUSH: u32 = 32; // (externref) -> ()
const IMP_COMPOUND_END: u32 = 33; // () -> externref
const IMP_COMPOUND_GET: u32 = 34; // (externref, externref) -> externref
const IMP_COMPOUND_LEN: u32 = 35; // (externref) -> externref
const IMP_PAIR_FIRST: u32 = 36; // (externref) -> externref
const IMP_PAIR_SECOND: u32 = 37; // (externref) -> externref
const NUM_IMPORTS: u32 = 38;

// --- Type indices (for the WASM type section) ---
const TY_VOID: u32 = 0; //  () -> ()
const TY_I32_I32: u32 = 1; //  (i32) -> i32
const TY_I32_I32_EXTERNREF: u32 = 2; //  (i32, i32) -> externref
const TY_I32_VOID: u32 = 3; //  (i32) -> ()
const TY_EXTERNREF_VOID: u32 = 4; //  (externref) -> ()
const TY_VOID_I32: u32 = 5; //  () -> i32
const TY_I32_I32_EXTERNREF_I32: u32 = 6; // (i32, i32, externref) -> i32
const TY_I32_I32_I32_I32: u32 = 7; //  (i32, i32, i32) -> i32
const TY_I64_EXTERNREF: u32 = 8; //  (i64) -> externref
const TY_I32_EXTERNREF: u32 = 9; //  (i32) -> externref
const TY_BINOP: u32 = 10; // (externref, externref) -> externref
const TY_UNOP: u32 = 11; // (externref) -> externref
const TY_CMP: u32 = 12; // (externref, externref) -> i32
const TY_QUADOP: u32 = 13; // (externref, externref, externref, externref) -> externref
const TY_VOID_EXTERNREF: u32 = 14; // () -> externref

/// The compiled output of the code generator.
pub struct CompiledModule {
    /// The WASM bytecode.
    pub wasm: Vec<u8>,
    /// String constants table (index 0 = StringId(1), etc.).
    pub strings: Vec<String>,
    /// Name constants table (index 0 = NameId(1), etc.).
    pub names: Vec<String>,
}

/// Backend strategy for implementing physical operations.
pub trait Backend {
    /// Emits code to start a scan. Pushes iter_id (i32) to stack.
    fn emit_scan_start(&self, func: &mut Function, rel_name: &str);

    /// Emits code to start a delta scan (new facts only).
    fn emit_scan_delta_start(&self, func: &mut Function, rel_name: &str);

    /// Emits code to start an indexed scan.
    /// Expects key (externref) on stack. Pushes iter_id (i32) to stack.
    fn emit_scan_index_start(&self, func: &mut Function, rel_name: &str, col_idx: u32);

    /// Emits code to start an aggregation scan.
    fn emit_scan_aggregate_start(&self, func: &mut Function, rel_name: &str, ptr: i32, len: i32);

    /// Emits code to get next tuple. Pushes tuple_ptr (i32) to stack.
    fn emit_scan_next(&self, func: &mut Function, iter_local: u32);

    /// Emits code to get column value from tuple. Pushes externref to stack.
    fn emit_get_col(&self, func: &mut Function, tuple_local: u32, col_idx: u32);

    /// Emits code to begin a multi-column insert.
    fn emit_insert_begin(&self, func: &mut Function, rel_name: &str);

    /// Emits code to push one column value (externref on stack) into the pending tuple.
    fn emit_insert_push(&self, func: &mut Function);

    /// Emits code to finalize the insert.
    fn emit_insert_end(&self, func: &mut Function);

    /// Emits code to merge deltas. Returns 1 if changes, 0 if not (i32 on stack).
    fn emit_merge_deltas(&self, func: &mut Function);

    /// Emits code to log a value (externref in local).
    fn emit_debuglog(&self, func: &mut Function, val_local: u32);
}

fn djb2_hash(name: &str) -> u32 {
    let mut hash: u32 = 5381;
    for c in name.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u32);
    }
    hash
}

pub struct WasmImportsBackend;

impl Backend for WasmImportsBackend {
    fn emit_scan_start(&self, func: &mut Function, rel_name: &str) {
        func.instruction(&Instruction::I32Const(djb2_hash(rel_name) as i32));
        func.instruction(&Instruction::Call(IMP_SCAN_START));
    }

    fn emit_scan_delta_start(&self, func: &mut Function, rel_name: &str) {
        func.instruction(&Instruction::I32Const(djb2_hash(rel_name) as i32));
        func.instruction(&Instruction::Call(IMP_SCAN_DELTA_START));
    }

    fn emit_scan_index_start(&self, func: &mut Function, rel_name: &str, col_idx: u32) {
        // Key (externref) is on stack. Save to Local 0 (externref scratch) to reorder.
        func.instruction(&Instruction::LocalSet(0));
        func.instruction(&Instruction::I32Const(djb2_hash(rel_name) as i32));
        func.instruction(&Instruction::I32Const(col_idx as i32));
        func.instruction(&Instruction::LocalGet(0));
        func.instruction(&Instruction::Call(IMP_SCAN_INDEX_START));
    }

    fn emit_scan_aggregate_start(&self, func: &mut Function, rel_name: &str, ptr: i32, len: i32) {
        func.instruction(&Instruction::I32Const(djb2_hash(rel_name) as i32));
        func.instruction(&Instruction::I32Const(ptr));
        func.instruction(&Instruction::I32Const(len));
        func.instruction(&Instruction::Call(IMP_SCAN_AGG_START));
    }

    fn emit_scan_next(&self, func: &mut Function, iter_local: u32) {
        func.instruction(&Instruction::LocalGet(iter_local));
        func.instruction(&Instruction::Call(IMP_SCAN_NEXT));
    }

    fn emit_get_col(&self, func: &mut Function, tuple_local: u32, col_idx: u32) {
        func.instruction(&Instruction::LocalGet(tuple_local));
        func.instruction(&Instruction::I32Const(col_idx as i32));
        func.instruction(&Instruction::Call(IMP_GET_COL));
    }

    fn emit_insert_begin(&self, func: &mut Function, rel_name: &str) {
        func.instruction(&Instruction::I32Const(djb2_hash(rel_name) as i32));
        func.instruction(&Instruction::Call(IMP_INSERT_BEGIN));
    }

    fn emit_insert_push(&self, func: &mut Function) {
        // externref value is already on the stack
        func.instruction(&Instruction::Call(IMP_INSERT_PUSH));
    }

    fn emit_insert_end(&self, func: &mut Function) {
        func.instruction(&Instruction::Call(IMP_INSERT_END));
    }

    fn emit_merge_deltas(&self, func: &mut Function) {
        func.instruction(&Instruction::Call(IMP_MERGE_DELTAS));
    }

    fn emit_debuglog(&self, func: &mut Function, val_local: u32) {
        func.instruction(&Instruction::LocalGet(val_local));
        func.instruction(&Instruction::Call(IMP_DEBUGLOG));
    }
}

pub struct Codegen<'a, B: Backend> {
    ir: &'a mut Ir,
    stratified: Option<&'a StratifiedProgram<'a>>,
    backend: B,
}

struct FuncContext {
    var_map: HashMap<NameId, u32>,
    next_local: u32,
    iter_base: u32,
    iter_offset: u32,
}

impl<'a, B: Backend> Codegen<'a, B> {
    pub fn new(ir: &'a mut Ir, backend: B) -> Self {
        Self {
            ir,
            stratified: None,
            backend,
        }
    }

    pub fn new_with_stratified(
        ir: &'a mut Ir,
        stratified: &'a StratifiedProgram<'a>,
        backend: B,
    ) -> Self {
        Self {
            ir,
            stratified: Some(stratified),
            backend,
        }
    }

    pub fn generate(&mut self) -> CompiledModule {
        let mut module = Module::new();

        // 1. Types
        let mut types = TypeSection::new();
        // T0: () -> ()
        types.ty().function(vec![], vec![]);
        // T1: (i32) -> i32
        types.ty().function(vec![ValType::I32], vec![ValType::I32]);
        // T2: (i32, i32) -> externref
        types
            .ty()
            .function(vec![ValType::I32, ValType::I32], vec![ValType::EXTERNREF]);
        // T3: (i32) -> ()
        types.ty().function(vec![ValType::I32], vec![]);
        // T4: (externref) -> ()
        types
            .ty()
            .function(vec![ValType::EXTERNREF], vec![]);
        // T5: () -> i32
        types.ty().function(vec![], vec![ValType::I32]);
        // T6: (i32, i32, externref) -> i32
        types.ty().function(
            vec![ValType::I32, ValType::I32, ValType::EXTERNREF],
            vec![ValType::I32],
        );
        // T7: (i32, i32, i32) -> i32
        types.ty().function(
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        );
        // T8: (i64) -> externref
        types
            .ty()
            .function(vec![ValType::I64], vec![ValType::EXTERNREF]);
        // T9: (i32) -> externref
        types
            .ty()
            .function(vec![ValType::I32], vec![ValType::EXTERNREF]);
        // T10: (externref, externref) -> externref
        types.ty().function(
            vec![ValType::EXTERNREF, ValType::EXTERNREF],
            vec![ValType::EXTERNREF],
        );
        // T11: (externref) -> externref
        types
            .ty()
            .function(vec![ValType::EXTERNREF], vec![ValType::EXTERNREF]);
        // T12: (externref, externref) -> i32
        types.ty().function(
            vec![ValType::EXTERNREF, ValType::EXTERNREF],
            vec![ValType::I32],
        );
        // T13: (externref, externref, externref, externref) -> externref
        types.ty().function(
            vec![
                ValType::EXTERNREF,
                ValType::EXTERNREF,
                ValType::EXTERNREF,
                ValType::EXTERNREF,
            ],
            vec![ValType::EXTERNREF],
        );
        // T14: () -> externref
        types
            .ty()
            .function(vec![], vec![ValType::EXTERNREF]);
        module.section(&types);

        // 2. Imports
        let mut imports = ImportSection::new();
        if std::any::type_name::<B>() == std::any::type_name::<WasmImportsBackend>() {
            imports.import("env", "scan_start", EntityType::Function(TY_I32_I32));
            imports.import("env", "scan_next", EntityType::Function(TY_I32_I32));
            imports.import("env", "get_col", EntityType::Function(TY_I32_I32_EXTERNREF));
            imports.import("env", "insert_begin", EntityType::Function(TY_I32_VOID));
            imports.import("env", "insert_push", EntityType::Function(TY_EXTERNREF_VOID));
            imports.import("env", "insert_end", EntityType::Function(TY_VOID));
            imports.import(
                "env",
                "scan_delta_start",
                EntityType::Function(TY_I32_I32),
            );
            imports.import("env", "merge_deltas", EntityType::Function(TY_VOID_I32));
            imports.import("env", "debuglog", EntityType::Function(TY_EXTERNREF_VOID));
            imports.import(
                "env",
                "scan_index_start",
                EntityType::Function(TY_I32_I32_EXTERNREF_I32),
            );
            imports.import(
                "env",
                "scan_aggregate_start",
                EntityType::Function(TY_I32_I32_I32_I32),
            );
            imports.import(
                "env",
                "const_number",
                EntityType::Function(TY_I64_EXTERNREF),
            );
            imports.import(
                "env",
                "const_float",
                EntityType::Function(TY_I64_EXTERNREF),
            );
            imports.import(
                "env",
                "const_string",
                EntityType::Function(TY_I32_EXTERNREF),
            );
            imports.import(
                "env",
                "const_name",
                EntityType::Function(TY_I32_EXTERNREF),
            );
            imports.import(
                "env",
                "const_time",
                EntityType::Function(TY_I64_EXTERNREF),
            );
            imports.import(
                "env",
                "const_duration",
                EntityType::Function(TY_I64_EXTERNREF),
            );
            imports.import("env", "val_add", EntityType::Function(TY_BINOP));
            imports.import("env", "val_sub", EntityType::Function(TY_BINOP));
            imports.import("env", "val_mul", EntityType::Function(TY_BINOP));
            imports.import("env", "val_div", EntityType::Function(TY_BINOP));
            imports.import("env", "val_sqrt", EntityType::Function(TY_UNOP));
            imports.import("env", "val_eq", EntityType::Function(TY_CMP));
            imports.import("env", "val_neq", EntityType::Function(TY_CMP));
            imports.import("env", "val_lt", EntityType::Function(TY_CMP));
            imports.import("env", "val_le", EntityType::Function(TY_CMP));
            imports.import("env", "val_gt", EntityType::Function(TY_CMP));
            imports.import("env", "val_ge", EntityType::Function(TY_CMP));
            imports.import("env", "str_concat", EntityType::Function(TY_BINOP));
            imports.import(
                "env",
                "str_replace",
                EntityType::Function(TY_QUADOP),
            );
            imports.import(
                "env",
                "val_to_string",
                EntityType::Function(TY_UNOP),
            );
            imports.import(
                "env",
                "compound_begin",
                EntityType::Function(TY_I32_VOID),
            );
            imports.import(
                "env",
                "compound_push",
                EntityType::Function(TY_EXTERNREF_VOID),
            );
            imports.import(
                "env",
                "compound_end",
                EntityType::Function(TY_VOID_EXTERNREF),
            );
            imports.import(
                "env",
                "compound_get",
                EntityType::Function(TY_BINOP),
            );
            imports.import(
                "env",
                "compound_len",
                EntityType::Function(TY_UNOP),
            );
            imports.import("env", "pair_first", EntityType::Function(TY_UNOP));
            imports.import("env", "pair_second", EntityType::Function(TY_UNOP));
        }
        module.section(&imports);

        // 3. Functions
        let mut functions = FunctionSection::new();
        functions.function(TY_VOID); // run function
        module.section(&functions);

        // 3b. Memory (for aggregate descriptions)
        let mut memories = MemorySection::new();
        memories.memory(wasm_encoder::MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        // 4. Exports
        let mut exports = ExportSection::new();
        exports.export("run", ExportKind::Func, NUM_IMPORTS);
        exports.export("memory", ExportKind::Memory, 0);
        module.section(&exports);

        // 5. Code — plan all rules
        let mut codes = CodeSection::new();

        let mut ops = Vec::new();
        let mut loop_ops = Vec::new();

        if let Some(stratified) = self.stratified {
            let arena = stratified.arena();
            for stratum in stratified.strata() {
                use fxhash::FxHashSet;
                let mut stratum_pred_names = FxHashSet::default();
                for pred in &stratum {
                    if let Some(name) = arena.predicate_name(*pred) {
                        stratum_pred_names.insert(name);
                    }
                }

                let mut rule_ids = Vec::new();
                for (i, inst) in self.ir.insts.iter().enumerate() {
                    if let Inst::Rule { head, .. } = inst
                        && let Inst::Atom { predicate, .. } = self.ir.get(*head)
                    {
                        let head_name = self.ir.resolve_name(*predicate);
                        if stratum_pred_names.contains(head_name) {
                            rule_ids.push(InstId::new(i));
                        }
                    }
                }

                if rule_ids.is_empty() {
                    continue;
                }

                let mut is_recursive = false;
                for &rule_id in &rule_ids {
                    if let Inst::Rule { premises, .. } = self.ir.get(rule_id) {
                        for &premise in premises {
                            if let Inst::Atom { predicate, .. } = self.ir.get(premise) {
                                let pred_name = self.ir.resolve_name(*predicate);
                                if stratum_pred_names.contains(pred_name) {
                                    is_recursive = true;
                                    break;
                                }
                            }
                        }
                    }
                    if is_recursive {
                        break;
                    }
                }

                if !is_recursive {
                    for rule_id in rule_ids {
                        let planner = Planner::new(self.ir);
                        if let Ok(op) = planner.plan_rule(rule_id) {
                            ops.push(op);
                        }
                    }
                } else {
                    // Recursive stratum: initial step + loop
                    for &rule_id in &rule_ids {
                        let planner = Planner::new(self.ir);
                        if let Ok(op) = planner.plan_rule(rule_id) {
                            ops.push(op);
                        }
                    }

                    let loop_start_idx = ops.len();

                    for &rule_id in &rule_ids {
                        let premises = if let Inst::Rule { premises, .. } = self.ir.get(rule_id) {
                            premises.clone()
                        } else {
                            continue;
                        };

                        for &premise in &premises {
                            let (predicate, pred_name) =
                                if let Inst::Atom { predicate, .. } = self.ir.get(premise) {
                                    (*predicate, self.ir.resolve_name(*predicate).to_string())
                                } else {
                                    continue;
                                };

                            if stratum_pred_names.contains(pred_name.as_str()) {
                                let planner = Planner::new(self.ir).with_delta(predicate);
                                if let Ok(op) = planner.plan_rule(rule_id) {
                                    ops.push(op);
                                }
                            }
                        }
                    }
                    let loop_end_idx = ops.len();
                    loop_ops.push((loop_start_idx, loop_end_idx));
                }
            }
        } else {
            // Naive / no stratification
            let rule_ids: Vec<_> = self
                .ir
                .insts
                .iter()
                .enumerate()
                .filter_map(|(i, inst)| {
                    if let Inst::Rule { .. } = inst {
                        Some(InstId::new(i))
                    } else {
                        None
                    }
                })
                .collect();

            for rule_id in &rule_ids {
                let planner = Planner::new(self.ir);
                if let Ok(op) = planner.plan_rule(*rule_id) {
                    ops.push(op);
                }
            }
        }

        // Locals: [0] externref scratch, [1] i32 tuple_ptr, [2..] externref vars, then i32 iters
        let mut locals = vec![
            (1, ValType::EXTERNREF), // Local 0: externref scratch
            (1, ValType::I32),       // Local 1: tuple_ptr
        ];

        let mut ctx = FuncContext {
            var_map: HashMap::new(),
            next_local: 2,
            iter_base: 0,
            iter_offset: 0,
        };

        // Pass 1: Collect vars and count iterators
        let mut total_iter_count = 0;
        for op in &ops {
            total_iter_count += Self::collect_vars(op, &mut ctx);
        }

        // Variable locals (externref)
        for _ in 0..ctx.var_map.len() {
            locals.push((1, ValType::EXTERNREF));
        }

        // Iterator locals (i32)
        ctx.iter_base = ctx.next_local;
        for _ in 0..total_iter_count {
            locals.push((1, ValType::I32));
        }

        let mut run_func = Function::new(locals);

        // Pass 2: Emit code
        let mut current_op_idx = 0;
        let mut loop_iter = loop_ops.into_iter();
        let mut next_loop = loop_iter.next();

        while current_op_idx < ops.len() {
            if let Some((start, end)) = next_loop
                && current_op_idx == start
            {
                // Merge deltas from initial step
                self.backend.emit_merge_deltas(&mut run_func);
                run_func.instruction(&Instruction::Drop);

                // Start loop
                run_func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                while current_op_idx < end {
                    self.emit_op(&mut run_func, &ops[current_op_idx], &mut ctx);
                    current_op_idx += 1;
                }

                // Merge deltas & check termination
                self.backend.emit_merge_deltas(&mut run_func);
                run_func.instruction(&Instruction::BrIf(0));
                run_func.instruction(&Instruction::End);

                next_loop = loop_iter.next();
                continue;
            }

            self.emit_op(&mut run_func, &ops[current_op_idx], &mut ctx);
            current_op_idx += 1;
        }

        run_func.instruction(&Instruction::End);
        codes.function(&run_func);
        module.section(&codes);

        CompiledModule {
            wasm: module.finish(),
            strings: self.ir.string_store.values().to_vec(),
            names: self.ir.name_store.values().to_vec(),
        }
    }

    fn collect_vars(op: &Op, ctx: &mut FuncContext) -> usize {
        let mut count = 0;
        match op {
            Op::Iterate { source, body } => {
                count += 1;
                match source {
                    DataSource::Scan { vars, .. }
                    | DataSource::ScanDelta { vars, .. }
                    | DataSource::IndexLookup { vars, .. } => {
                        for v in vars {
                            if !ctx.var_map.contains_key(v) {
                                ctx.var_map.insert(*v, ctx.next_local);
                                ctx.next_local += 1;
                            }
                        }
                    }
                }
                count += Self::collect_vars(body, ctx);
            }
            Op::Let { var, body, .. } => {
                if !ctx.var_map.contains_key(var) {
                    ctx.var_map.insert(*var, ctx.next_local);
                    ctx.next_local += 1;
                }
                count += Self::collect_vars(body, ctx);
            }
            Op::Filter { body, .. } => {
                count += Self::collect_vars(body, ctx);
            }
            Op::Seq(ops) => {
                for o in ops {
                    count += Self::collect_vars(o, ctx);
                }
            }
            Op::GroupBy {
                body,
                vars,
                aggregates,
                ..
            } => {
                for v in vars {
                    if !ctx.var_map.contains_key(v) {
                        ctx.var_map.insert(*v, ctx.next_local);
                        ctx.next_local += 1;
                    }
                }
                for agg in aggregates {
                    if !ctx.var_map.contains_key(&agg.var) {
                        ctx.var_map.insert(agg.var, ctx.next_local);
                        ctx.next_local += 1;
                    }
                }
                count += Self::collect_vars(body, ctx);
            }
            _ => {}
        }
        count
    }

    fn emit_op(&self, func: &mut Function, op: &Op, ctx: &mut FuncContext) {
        match op {
            Op::Iterate { source, body } => {
                match source {
                    DataSource::Scan { relation, vars }
                    | DataSource::ScanDelta { relation, vars } => {
                        let iter_local = ctx.iter_base + ctx.iter_offset;
                        ctx.iter_offset += 1;

                        let rel_name = self.ir.resolve_name(*relation);
                        if let DataSource::ScanDelta { .. } = source {
                            self.backend.emit_scan_delta_start(func, rel_name);
                        } else {
                            self.backend.emit_scan_start(func, rel_name);
                        }
                        func.instruction(&Instruction::LocalSet(iter_local));

                        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                        func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                        self.backend.emit_scan_next(func, iter_local);
                        func.instruction(&Instruction::LocalTee(1));

                        func.instruction(&Instruction::I32Eqz);
                        func.instruction(&Instruction::BrIf(1));

                        // Bind vars: get_col returns externref
                        for (i, var) in vars.iter().enumerate() {
                            if let Some(&local_idx) = ctx.var_map.get(var) {
                                self.backend.emit_get_col(func, 1, i as u32);
                                func.instruction(&Instruction::LocalSet(local_idx));
                            }
                        }

                        self.emit_op(func, body, ctx);

                        func.instruction(&Instruction::Br(0));
                        func.instruction(&Instruction::End);
                        func.instruction(&Instruction::End);
                    }
                    DataSource::IndexLookup {
                        relation,
                        col_idx,
                        key,
                        vars,
                    } => {
                        let iter_local = ctx.iter_base + ctx.iter_offset;
                        ctx.iter_offset += 1;

                        let rel_name = self.ir.resolve_name(*relation);
                        self.emit_operand(func, key, ctx);
                        self.backend
                            .emit_scan_index_start(func, rel_name, *col_idx as u32);
                        func.instruction(&Instruction::LocalSet(iter_local));

                        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                        func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                        self.backend.emit_scan_next(func, iter_local);
                        func.instruction(&Instruction::LocalTee(1));

                        func.instruction(&Instruction::I32Eqz);
                        func.instruction(&Instruction::BrIf(1));

                        for (i, var) in vars.iter().enumerate() {
                            if let Some(&local_idx) = ctx.var_map.get(var) {
                                self.backend.emit_get_col(func, 1, i as u32);
                                func.instruction(&Instruction::LocalSet(local_idx));
                            }
                        }

                        self.emit_op(func, body, ctx);

                        func.instruction(&Instruction::Br(0));
                        func.instruction(&Instruction::End);
                        func.instruction(&Instruction::End);
                    }
                }
            }
            Op::GroupBy { .. } => {}
            Op::Filter { cond, body } => {
                self.emit_condition(func, cond, ctx);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                self.emit_op(func, body, ctx);
                func.instruction(&Instruction::End);
            }
            Op::Let { var, expr, body } => {
                self.emit_expr(func, expr, ctx);
                if let Some(&local_idx) = ctx.var_map.get(var) {
                    func.instruction(&Instruction::LocalSet(local_idx));
                } else {
                    func.instruction(&Instruction::Drop);
                }
                self.emit_op(func, body, ctx);
            }
            Op::Insert { relation, args } => {
                let rel_name = self.ir.resolve_name(*relation);
                self.backend.emit_insert_begin(func, rel_name);
                for arg in args {
                    self.emit_operand(func, arg, ctx);
                    self.backend.emit_insert_push(func);
                }
                self.backend.emit_insert_end(func);
            }
            Op::Seq(ops) => {
                for o in ops {
                    self.emit_op(func, o, ctx);
                }
            }
            _ => {}
        }
    }

    fn emit_condition(&self, func: &mut Function, cond: &Condition, ctx: &FuncContext) {
        match cond {
            Condition::Cmp { op, left, right } => {
                self.emit_operand(func, left, ctx);
                self.emit_operand(func, right, ctx);
                let import_idx = match op {
                    CmpOp::Eq => IMP_VAL_EQ,
                    CmpOp::Neq => IMP_VAL_NEQ,
                    CmpOp::Lt => IMP_VAL_LT,
                    CmpOp::Le => IMP_VAL_LE,
                    CmpOp::Gt => IMP_VAL_GT,
                    CmpOp::Ge => IMP_VAL_GE,
                };
                func.instruction(&Instruction::Call(import_idx));
            }
            _ => {
                func.instruction(&Instruction::I32Const(1));
            }
        }
    }

    fn emit_expr(&self, func: &mut Function, expr: &Expr, ctx: &FuncContext) {
        match expr {
            Expr::Value(op) => self.emit_operand(func, op, ctx),
            Expr::Call { function, args } => {
                let name = self.ir.resolve_name(*function);
                match name {
                    "fn:plus" | "fn:float:plus" => {
                        self.emit_binary_fold(func, args, ctx, IMP_VAL_ADD);
                    }
                    "fn:minus" | "fn:float:minus" => {
                        self.emit_binary_fold(func, args, ctx, IMP_VAL_SUB);
                    }
                    "fn:mult" | "fn:float:mult" => {
                        self.emit_binary_fold(func, args, ctx, IMP_VAL_MUL);
                    }
                    "fn:div" | "fn:float:div" => {
                        self.emit_binary_fold(func, args, ctx, IMP_VAL_DIV);
                    }
                    "fn:sqrt" => {
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_VAL_SQRT));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    // --- String functions ---
                    "fn:string:concat" => {
                        self.emit_binary_fold(func, args, ctx, IMP_STR_CONCAT);
                    }
                    "fn:string:replace" => {
                        if args.len() == 4 {
                            for arg in args {
                                self.emit_operand(func, arg, ctx);
                            }
                            func.instruction(&Instruction::Call(IMP_STR_REPLACE));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    "fn:number:to_string" | "fn:float64:to_string" | "fn:name:to_string" => {
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_VAL_TO_STRING));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    // --- Compound constructors ---
                    "fn:list" => {
                        func.instruction(&Instruction::I32Const(0)); // kind=List
                        func.instruction(&Instruction::Call(IMP_COMPOUND_BEGIN));
                        for arg in args {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_PUSH));
                        }
                        func.instruction(&Instruction::Call(IMP_COMPOUND_END));
                    }
                    "fn:pair" => {
                        func.instruction(&Instruction::I32Const(1)); // kind=Pair
                        func.instruction(&Instruction::Call(IMP_COMPOUND_BEGIN));
                        for arg in args {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_PUSH));
                        }
                        func.instruction(&Instruction::Call(IMP_COMPOUND_END));
                    }
                    "fn:map" => {
                        func.instruction(&Instruction::I32Const(2)); // kind=Map
                        func.instruction(&Instruction::Call(IMP_COMPOUND_BEGIN));
                        for arg in args {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_PUSH));
                        }
                        func.instruction(&Instruction::Call(IMP_COMPOUND_END));
                    }
                    "fn:struct" => {
                        func.instruction(&Instruction::I32Const(3)); // kind=Struct
                        func.instruction(&Instruction::Call(IMP_COMPOUND_BEGIN));
                        for arg in args {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_PUSH));
                        }
                        func.instruction(&Instruction::Call(IMP_COMPOUND_END));
                    }
                    // --- Compound accessors ---
                    "fn:list:get" | "fn:map:get" | "fn:struct:get" => {
                        if args.len() == 2 {
                            self.emit_operand(func, &args[0], ctx);
                            self.emit_operand(func, &args[1], ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_GET));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    "fn:len" | "fn:list:len" | "fn:struct:len" | "fn:map:len" => {
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_LEN));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    "fn:pair:first" => {
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_PAIR_FIRST));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    "fn:pair:second" => {
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_PAIR_SECOND));
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    "fn:map:keys" | "fn:map:values" | "fn:struct:values" => {
                        // These return compounds — delegate to host via compound_get
                        // with a special key convention. For now, treat as unary.
                        if let Some(arg) = args.first() {
                            self.emit_operand(func, arg, ctx);
                            func.instruction(&Instruction::Call(IMP_COMPOUND_LEN));
                            // TODO: proper keys/values extraction
                        } else {
                            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                        }
                    }
                    _ => {
                        // Unknown function: drop all args, push null
                        for arg in args {
                            self.emit_operand(func, arg, ctx);
                        }
                        for _ in 0..args.len() {
                            func.instruction(&Instruction::Drop);
                        }
                        func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                    }
                }
            }
        }
    }

    /// Emit a left-fold of binary operations: f(f(f(a, b), c), d)
    fn emit_binary_fold(
        &self,
        func: &mut Function,
        args: &[Operand],
        ctx: &FuncContext,
        import_idx: u32,
    ) {
        if args.is_empty() {
            func.instruction(&Instruction::RefNull(HeapType::EXTERN));
            return;
        }
        self.emit_operand(func, &args[0], ctx);
        for arg in &args[1..] {
            self.emit_operand(func, arg, ctx);
            func.instruction(&Instruction::Call(import_idx));
        }
    }

    fn emit_operand(&self, func: &mut Function, op: &Operand, ctx: &FuncContext) {
        match op {
            Operand::Var(v) => {
                if let Some(&idx) = ctx.var_map.get(v) {
                    func.instruction(&Instruction::LocalGet(idx));
                } else {
                    func.instruction(&Instruction::RefNull(HeapType::EXTERN));
                }
            }
            Operand::Const(c) => match c {
                Constant::Number(n) => {
                    func.instruction(&Instruction::I64Const(*n));
                    func.instruction(&Instruction::Call(IMP_CONST_NUMBER));
                }
                Constant::Float(f) => {
                    func.instruction(&Instruction::I64Const(f.to_bits() as i64));
                    func.instruction(&Instruction::Call(IMP_CONST_FLOAT));
                }
                Constant::String(sid) => {
                    func.instruction(&Instruction::I32Const(sid.0.get() as i32));
                    func.instruction(&Instruction::Call(IMP_CONST_STRING));
                }
                Constant::Name(nid) => {
                    func.instruction(&Instruction::I32Const(nid.0.get() as i32));
                    func.instruction(&Instruction::Call(IMP_CONST_NAME));
                }
                Constant::Time(t) => {
                    func.instruction(&Instruction::I64Const(*t));
                    func.instruction(&Instruction::Call(IMP_CONST_TIME));
                }
                Constant::Duration(d) => {
                    func.instruction(&Instruction::I64Const(*d));
                    func.instruction(&Instruction::Call(IMP_CONST_DURATION));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangle_analysis::LoweringContext;
    use mangle_ast as ast;

    #[test]
    fn test_codegen_with_imports() {
        let arena = ast::Arena::new_with_global_interner();
        let foo = arena.predicate_sym("foo", Some(1));
        let bar = arena.predicate_sym("bar", Some(1));
        let x = arena.variable("X");

        let clause = ast::Clause {
            head: arena.atom(foo, &[x]),
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

        assert!(!compiled.wasm.is_empty());

        use wasmparser::Payload;
        let parser = wasmparser::Parser::new(0);
        let mut found_scan_start = false;
        let mut found_get_col = false;
        let mut found_const_number = false;
        let mut found_code = false;

        for payload in parser.parse_all(&compiled.wasm) {
            match payload.expect("parsing failed") {
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import.expect("import failed");
                        if import.module == "env" {
                            match import.name {
                                "scan_start" => found_scan_start = true,
                                "get_col" => found_get_col = true,
                                "const_number" => found_const_number = true,
                                _ => {}
                            }
                        }
                    }
                }
                Payload::CodeSectionEntry(_) => {
                    found_code = true;
                }
                _ => {}
            }
        }
        assert!(found_scan_start, "scan_start import not found");
        assert!(found_get_col, "get_col import not found");
        assert!(found_const_number, "const_number import not found");
        assert!(found_code, "code section empty");
    }

    #[test]
    fn test_codegen_string_constant() {
        let arena = ast::Arena::new_with_global_interner();
        let foo = arena.predicate_sym("foo", Some(1));
        let hello = arena.const_(ast::Const::String("hello"));

        let clause = ast::Clause {
            head: arena.atom(foo, &[hello]),
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

        assert!(!compiled.wasm.is_empty());
        // Verify the string table contains "hello"
        assert!(
            compiled.strings.contains(&"hello".to_string()),
            "string table should contain 'hello'"
        );

        // Verify WASM is valid
        let parser = wasmparser::Parser::new(0);
        let mut found_const_string = false;
        for payload in parser.parse_all(&compiled.wasm) {
            match payload.expect("parsing failed") {
                wasmparser::Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import.expect("import failed");
                        if import.name == "const_string" {
                            found_const_string = true;
                        }
                    }
                }
                _ => {}
            }
        }
        assert!(found_const_string, "const_string import not found");
    }
}
