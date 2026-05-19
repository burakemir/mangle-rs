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

//! The Mangle Driver.
//!
//! This crate acts as the orchestrator for the Mangle compiler pipeline.
//! It connects parsing, analysis, and execution components to provide a
//! high-level API for running Mangle programs.
//!
//! # Execution Architecture
//!
//! Mangle supports multiple execution strategies:
//!
//! 1.  **Reference Implementation (Legacy)**: A naive bottom-up evaluator that operates directly on the AST.
//!     This is implemented in the `mangle-engine` crate and serves as a correctness baseline.
//!     It is not used by this driver.
//!
//! 2.  **Interpreter (Default)**: A high-performance interpreter that executes the Mangle Intermediate Representation (IR).
//!     The driver compiles source code to IR and then executes it using `mangle-interpreter`.
//!
//! 3.  **WASM Compilation**: The IR can be compiled to WebAssembly (WASM) for execution in browsers or
//!     WASM runtimes. This is handled by `mangle-codegen`.
//!
//! # Key Responsibilities
//!
//! *   **Compilation**: Parsing source code and lowering it to the Intermediate Representation (IR).
//! *   **Stratification**: Analyzing dependencies between predicates to determine the correct
//!     evaluation order (handling negation and recursion). This is implemented in [`Program`].
//! *   **Execution**: Running the compiled plan using the [`mangle_interpreter`].
//! *   **Codegen**: Generating WASM modules from the IR.
//!
//! # Example
//!
//! ```rust
//! use mangle_ast::Arena;
//! use mangle_driver::{compile, execute};
//!
//! let arena = Arena::new_with_global_interner();
//! let source = "p(1). q(X) :- p(X).";
//!
//! // 1. Compile
//! let (mut ir, stratified) = compile(source, &arena).expect("compilation failed");
//!
//! // 2. Execute
//! let store = Box::new(mangle_interpreter::MemStore::new());
//! let interpreter = execute(&mut ir, &stratified, store).expect("execution failed");
//! ```

use anyhow::{Result, anyhow};
use ast::Arena;
use fxhash::FxHashSet;
use mangle_analysis::{BoundsChecker, LoweringContext, Planner, Program, StratifiedProgram, rewrite_unit};
use mangle_ast as ast;
use mangle_codegen::{Codegen, WasmImportsBackend};
use mangle_interpreter::{Interpreter, Store};
use mangle_ir::{Inst, InstId, Ir};
use mangle_parse::Parser;

/// Compiles source code into the Mangle Intermediate Representation (IR).
///
/// This function performs:
/// 1.  Parsing of the source string into an AST.
/// 2.  **Renaming**: Applies package rewrites to support module namespacing.
/// 3.  **Stratification**: Orders the evaluation of rules.
/// 4.  **Lowering**: Converts the AST into the flat IR.
///
/// Returns a tuple containing the IR and the stratification info (which dictates
/// the order of execution).
pub fn compile<'a>(source: &str, arena: &'a Arena) -> Result<(Ir, StratifiedProgram<'a>)> {
    compile_units(&[source], arena)
}

/// Compiles multiple source units into the Mangle Intermediate Representation (IR).
///
/// Each source string is parsed into a separate AST unit, renamed independently
/// (handling Package/Use directives), and then merged into a single unit for
/// stratification and lowering.
///
/// This enables multi-unit compilation where one unit can declare a `Package`
/// and another can `Use` it with qualified predicate references.
pub fn compile_units<'a>(sources: &[&str], arena: &'a Arena) -> Result<(Ir, StratifiedProgram<'a>)> {
    // Parse and rename each source unit independently
    let mut all_decls: Vec<&'a ast::Decl<'a>> = Vec::new();
    let mut all_clauses: Vec<&'a ast::Clause<'a>> = Vec::new();

    for (i, source) in sources.iter().enumerate() {
        let label = format!("source_{}", i);
        let mut parser = Parser::new(arena, source.as_bytes(), arena.alloc_str(&label));
        parser.next_token().map_err(|e| anyhow!(e))?;
        let unit = parser.parse_unit()?;

        let rewritten = rewrite_unit(arena, unit);
        all_decls.extend_from_slice(rewritten.decls);
        all_clauses.extend_from_slice(rewritten.clauses);
    }

    // Build the merged unit
    let merged_unit = ast::Unit {
        decls: arena.alloc_slice_copy(&all_decls),
        clauses: arena.alloc_slice_copy(&all_clauses),
    };
    let unit = &merged_unit;

    let mut program = Program::new(arena);
    let mut all_preds = FxHashSet::default();
    let mut idb_preds = FxHashSet::default();

    for clause in unit.clauses {
        program.add_clause(arena, clause);
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

    let stratified = program.stratify().map_err(|e| anyhow!(e))?;

    let ctx = LoweringContext::new(arena);
    let mut ir = ctx.lower_unit(unit);

    // Bounds checking: validate facts and rules against declared type bounds.
    let mut bounds_checker = BoundsChecker::new(&mut ir);
    bounds_checker.check()?;

    Ok((ir, stratified))
}

/// Compiles the Intermediate Representation (IR) into a WebAssembly (WASM) module.
///
/// This uses the default `WasmImportsBackend` which expects certain host functions
/// to be available for data access. Returns a `CompiledModule` containing the WASM
/// bytecode and string/name tables needed by the host runtime.
pub fn compile_to_wasm(
    ir: &mut Ir,
    stratified: &StratifiedProgram,
) -> mangle_codegen::CompiledModule {
    let mut codegen = Codegen::new_with_stratified(ir, stratified, WasmImportsBackend);
    codegen.generate()
}

/// Executes a compiled Mangle program using the pure Rust interpreter.
///
/// This function:
/// 1.  Iterates through the strata defined in `StratifiedProgram`.
/// 2.  Identifies recursive predicates within each stratum.
/// 3.  Executes non-recursive strata once.
/// 4.  Executes recursive strata using a semi-naive evaluation loop.
///
/// Returns the `Interpreter` instance, which holds the final state (facts) of the execution.
pub fn execute<'a>(
    ir: &'a mut Ir,
    stratified: &StratifiedProgram<'a>,
    store: Box<dyn Store + 'a>,
) -> Result<Interpreter<'a>> {
    execute_with_options(ir, stratified, store, false)
}

/// Like [`execute`], but with provenance recording enabled. The
/// returned `Interpreter` carries a populated `ProvenanceRecorder`;
/// inspect via `Interpreter::into_provenance` or the matching `&self`
/// accessor.
pub fn execute_with_provenance<'a>(
    ir: &'a mut Ir,
    stratified: &StratifiedProgram<'a>,
    store: Box<dyn Store + 'a>,
) -> Result<Interpreter<'a>> {
    execute_with_options(ir, stratified, store, true)
}

fn execute_with_options<'a>(
    ir: &'a mut Ir,
    stratified: &StratifiedProgram<'a>,
    store: Box<dyn Store + 'a>,
    with_provenance: bool,
) -> Result<Interpreter<'a>> {
    let arena = stratified.arena();

    // 1. Pre-plan everything that needs mutable access to IR
    let mut strata_plans = Vec::new();

    for stratum in stratified.strata() {
        let mut stratum_pred_names = FxHashSet::default();
        for pred in &stratum {
            if let Some(name) = arena.predicate_name(*pred) {
                stratum_pred_names.insert(name);
            }
        }

        // Identify rules for this stratum
        let mut rule_ids = Vec::new();
        for (i, inst) in ir.insts.iter().enumerate() {
            if let Inst::Rule { head, .. } = inst
                && let Inst::Atom { predicate, .. } = ir.get(*head)
            {
                let head_name = ir.resolve_name(*predicate);
                if stratum_pred_names.contains(head_name) {
                    rule_ids.push(InstId::new(i));
                }
            }
        }

        if rule_ids.is_empty() {
            strata_plans.push(None);
            continue;
        }

        // Check if any rule in the stratum is recursive
        let mut is_recursive = false;
        for &rule_id in &rule_ids {
            if let Inst::Rule { premises, .. } = ir.get(rule_id) {
                for &premise in premises {
                    if let Inst::Atom { predicate, .. } = ir.get(premise) {
                        let pred_name = ir.resolve_name(*predicate);
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
            let mut ops = Vec::new();
            for rule_id in rule_ids {
                let planner = Planner::new(ir);
                ops.push(planner.plan_rule(rule_id)?);
            }
            strata_plans.push(Some(StratumPlan::NonRecursive(ops)));
        } else {
            let mut initial_ops = Vec::new();
            for &rule_id in &rule_ids {
                let planner = Planner::new(ir);
                initial_ops.push(planner.plan_rule(rule_id)?);
            }

            let mut delta_plans = Vec::new();
            for &rule_id in &rule_ids {
                let premises = if let Inst::Rule { premises, .. } = ir.get(rule_id) {
                    premises.clone()
                } else {
                    continue;
                };

                for &premise in &premises {
                    let (predicate, pred_name) =
                        if let Inst::Atom { predicate, .. } = ir.get(premise) {
                            (*predicate, ir.resolve_name(*predicate).to_string())
                        } else {
                            continue;
                        };

                    if stratum_pred_names.contains(pred_name.as_str()) {
                        let planner = Planner::new(ir).with_delta(predicate);
                        delta_plans.push(planner.plan_rule(rule_id)?);
                    }
                }
            }
            strata_plans.push(Some(StratumPlan::Recursive {
                initial_ops,
                delta_plans,
            }));
        }
    }

    // Collect temporal predicate names for coalescing
    let temporal_pred_names: Vec<String> = ir
        .temporal_predicates
        .iter()
        .map(|name_id| ir.resolve_name(*name_id).to_string())
        .collect();

    // 2. Now execute using the interpreter
    let mut interpreter = Interpreter::new(ir, store);
    if with_provenance {
        interpreter = interpreter.with_provenance();
    }

    // Initialize EDB relations
    for pred in stratified.extensional_preds() {
        if let Some(name) = arena.predicate_name(pred) {
            interpreter.store_mut().create_relation(name);
        }
    }

    for plan in strata_plans {
        match plan {
            Some(StratumPlan::NonRecursive(ops)) => {
                for op in ops {
                    interpreter.execute(&op)?;
                }
            }
            Some(StratumPlan::Recursive {
                initial_ops,
                delta_plans,
            }) => {
                for op in initial_ops {
                    interpreter.execute(&op)?;
                }
                interpreter.store_mut().merge_deltas();

                loop {
                    let mut changes = 0;
                    for op in &delta_plans {
                        changes += interpreter.execute(op)?;
                    }
                    if changes == 0 {
                        break;
                    }
                    interpreter.store_mut().merge_deltas();
                }
                // Coalesce temporal intervals after fixpoint converges
                for name in &temporal_pred_names {
                    interpreter.store_mut().coalesce_temporal(name);
                }
            }
            None => {}
        }
        interpreter.store_mut().merge_deltas();
        for name in &temporal_pred_names {
            interpreter.store_mut().coalesce_temporal(name);
        }
    }

    Ok(interpreter)
}

enum StratumPlan {
    NonRecursive(Vec<mangle_ir::physical::Op>),
    Recursive {
        initial_ops: Vec<mangle_ir::physical::Op>,
        delta_plans: Vec<mangle_ir::physical::Op>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangle_interpreter::{MemStore, Value};

    #[test]
    fn test_driver_e2e() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1).
            p(2).
            q(X) :- p(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        // Check results
        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert!(!facts.is_empty(), "relation q not found");

        let mut values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        values.sort();

        assert_eq!(values, vec![1, 2]);

        Ok(())
    }

    #[test]
    fn test_driver_e2e_with_package() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Package pkg!
            p(1).
            q(X) :- p(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        // Check results - predicates should be prefixed with "pkg."
        let facts: Vec<_> = interpreter
            .store()
            .scan("pkg.q")
            .expect("relation pkg.q not found")
            .collect();
        assert!(!facts.is_empty(), "relation pkg.q not found");

        let values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        assert_eq!(values, vec![1]);

        Ok(())
    }

    #[test]
    fn test_driver_let_transform() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1).
            p(2).
            q(Y) :- p(X) |> let Y = fn:plus(X, 10).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        let mut values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        values.sort();

        assert_eq!(values, vec![11, 12]);
        Ok(())
    }

    #[test]
    fn test_driver_aggregation() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1, 10).
            p(1, 20).
            p(2, 30).
            q(K, S) :- p(K, V) |> do fn:group_by(K); let S = fn:sum(V).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        let mut results: Vec<(i64, i64)> = facts
            .iter()
            .map(|t| {
                if let (Value::Number(k), Value::Number(s)) = (&t[0], &t[1]) {
                    (*k, *s)
                } else {
                    panic!("expected numbers");
                }
            })
            .collect();
        results.sort();

        assert_eq!(results, vec![(1, 30), (2, 30)]);
        Ok(())
    }

    #[test]
    fn test_driver_aggregation_count() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1, 10).
            p(1, 20).
            p(2, 30).
            q(K, C) :- p(K, V) |> do fn:group_by(K); let C = fn:count(V).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        let mut results: Vec<(i64, i64)> = facts
            .iter()
            .map(|t| {
                if let (Value::Number(k), Value::Number(c)) = (&t[0], &t[1]) {
                    (*k, *c)
                } else {
                    panic!("expected numbers");
                }
            })
            .collect();
        results.sort();

        assert_eq!(results, vec![(1, 2), (2, 1)]);
        Ok(())
    }

    #[test]
    fn test_driver_reachability() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            edge(1, 2).
            edge(2, 3).
            edge(3, 4).
            edge(4, 5).
            reachable(X, Y) :- edge(X, Y).
            reachable(X, Z) :- reachable(X, Y), edge(Y, Z).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reachable")
            .expect("reachable relation not found")
            .collect();
        assert_eq!(facts.len(), 10); // (1,2),(1,3),(1,4),(1,5), (2,3),(2,4),(2,5), (3,4),(3,5), (4,5)

        let mut reachable_from_1: Vec<i64> = facts
            .iter()
            .filter(|t| t[0] == Value::Number(1))
            .map(|t| match t[1] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        reachable_from_1.sort();
        assert_eq!(reachable_from_1, vec![2, 3, 4, 5]);

        Ok(())
    }

    #[test]
    fn test_name_constants() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            role(/role/admin).
            role(/role/user).
            role(/role/application).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("role")
            .expect("relation role not found")
            .collect();
        assert_eq!(facts.len(), 3);

        let mut names: Vec<String> = facts
            .iter()
            .map(|t| match &t[0] {
                Value::Name(s) => s.clone(),
                _ => panic!("expected name"),
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["/role/admin", "/role/application", "/role/user"]
        );

        Ok(())
    }

    #[test]
    fn test_inequality() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // Note: name constants like /role/application cannot appear immediately
        // before `.` because the scanner treats `.` as a name_char. Use string
        // constants or ensure a `)` separates the name from the clause terminator.
        let source = r#"
            role("admin").
            role("user").
            role("application").
            non_app_role(R) :- role(R), R != "application".
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("non_app_role")
            .expect("relation non_app_role not found")
            .collect();
        assert_eq!(facts.len(), 2);

        let mut names: Vec<String> = facts
            .iter()
            .map(|t| match &t[0] {
                Value::String(s) => s.clone(),
                _ => panic!("expected string"),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["admin", "user"]);

        Ok(())
    }

    #[test]
    fn test_negation() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            service("web").
            service("api").
            service("db").
            has_dep("web").
            has_dep("api").
            no_dep(S) :- service(S), !has_dep(S).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("no_dep")
            .expect("relation no_dep not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("db".to_string()));

        Ok(())
    }

    #[test]
    fn test_combined_name_ineq_negation() -> Result<()> {
        // Mini devops-like program exercising all features together
        let arena = Arena::new_with_global_interner();
        let source = r#"
            container("web", /status/running).
            container("api", /status/running).
            container("db", /status/stopped).
            depends_on("web", "db").
            depends_on("api", "db").

            running(Name) :- container(Name, /status/running).
            stopped(Name) :- container(Name, /status/stopped).
            has_running_dep(Name) :- depends_on(Name, Dep), running(Dep).
            needs_attention(Name) :- depends_on(Name, Dep), stopped(Dep).
            independent(Name) :- running(Name), !has_running_dep(Name).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        // Check running containers
        let running: Vec<_> = interpreter
            .store()
            .scan("running")
            .expect("relation running not found")
            .collect();
        assert_eq!(running.len(), 2);

        // Check stopped
        let stopped: Vec<_> = interpreter
            .store()
            .scan("stopped")
            .expect("relation stopped not found")
            .collect();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0][0], Value::String("db".to_string()));

        // Both web and api depend on db which is stopped
        let needs_attention: Vec<_> = interpreter
            .store()
            .scan("needs_attention")
            .expect("relation needs_attention not found")
            .collect();
        assert_eq!(needs_attention.len(), 2);

        // db is not running so nobody has a running dep
        // Both web and api are running and have no running deps
        let independent: Vec<_> = interpreter
            .store()
            .scan("independent")
            .expect("relation independent not found")
            .collect();
        assert_eq!(independent.len(), 2);

        Ok(())
    }

    #[test]
    fn test_join_with_constants_in_second_atom() -> Result<()> {
        // Regression: fresh_var used ir.insts.len() as counter, producing
        // duplicate NameIds for scan variables. This caused the second body
        // atom's columns to overwrite each other during IndexLookup execution.
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p("a", "x").
            q("a", "y").
            test(E) :- p(E, "x"), q(E, "y").
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("test")
            .expect("relation test not found")
            .collect();

        assert_eq!(facts.len(), 1, "expected 1 result, got {:?}", facts);
        assert_eq!(facts[0][0], Value::String("a".to_string()));

        Ok(())
    }

    #[test]
    fn test_join_constant_only_in_second_atom() -> Result<()> {
        // Simpler variant: constant only in second atom
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p("a", "x").
            q("a", "y").
            test(E, V) :- p(E, V), q(E, "y").
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("test")
            .expect("relation test not found")
            .collect();

        assert_eq!(facts.len(), 1, "expected 1 result, got {:?}", facts);
        assert_eq!(facts[0][0], Value::String("a".to_string()));
        assert_eq!(facts[0][1], Value::String("x".to_string()));

        Ok(())
    }

    #[test]
    fn test_compile_units_package_use() -> Result<()> {
        let arena = Arena::new_with_global_interner();

        let schema = r#"
            Package config_schema !
            Decl server_port(Port).
            Decl programs_dir(Path).
        "#;

        let config = r#"
            Use config_schema !
            config_schema.server_port(8090).
            config_schema.programs_dir("/programs").
        "#;

        let (mut ir, stratified) = compile_units(&[schema, config], &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        // Query the qualified predicate
        let port_facts: Vec<_> = interpreter
            .store()
            .scan("config_schema.server_port")
            .expect("relation config_schema.server_port not found")
            .collect();
        assert_eq!(port_facts.len(), 1);
        assert_eq!(port_facts[0][0], Value::Number(8090));

        let dir_facts: Vec<_> = interpreter
            .store()
            .scan("config_schema.programs_dir")
            .expect("relation config_schema.programs_dir not found")
            .collect();
        assert_eq!(dir_facts.len(), 1);
        assert_eq!(dir_facts[0][0], Value::String("/programs".to_string()));

        Ok(())
    }

    #[test]
    fn test_less_than_comparison() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            num(8.1). num(42). num(99.5).
            big(X) :- num(X), 85 < X .
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("big")
            .expect("relation big not found")
            .collect();
        assert_eq!(facts.len(), 1, "expected 1 result, got {:?}", facts);
        assert_eq!(facts[0][0], Value::Float(99.5));

        Ok(())
    }

    #[test]
    fn test_less_equal_comparison() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            num(10). num(50). num(85). num(99).
            up_to_85(X) :- num(X), X <= 85 .
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("up_to_85")
            .expect("relation up_to_85 not found")
            .collect();
        assert_eq!(facts.len(), 3, "expected 3 results, got {:?}", facts);

        Ok(())
    }

    #[test]
    fn test_compile_to_wasm() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1).
            q(X) :- p(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let compiled = compile_to_wasm(&mut ir, &stratified);

        // Basic check that we generated something that looks like WASM
        assert!(!compiled.wasm.is_empty());
        assert_eq!(&compiled.wasm[0..4], b"\0asm"); // WASM magic header

        Ok(())
    }

    #[test]
    fn test_greater_than_comparison() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            num(10). num(50). num(85). num(99).
            big(X) :- num(X), X > 50 .
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("big")
            .expect("relation big not found")
            .collect();
        assert_eq!(facts.len(), 2, "expected 2 results, got {:?}", facts);

        let mut values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec![85, 99]);

        Ok(())
    }

    #[test]
    fn test_greater_equal_comparison() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            num(10). num(50). num(85). num(99).
            at_least_85(X) :- num(X), X >= 85 .
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("at_least_85")
            .expect("relation at_least_85 not found")
            .collect();
        assert_eq!(facts.len(), 2, "expected 2 results, got {:?}", facts);

        Ok(())
    }

    #[test]
    fn test_variadic_arithmetic() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1).
            p(2).
            q(Y) :- p(X) |> let Y = fn:plus(X, 10, 100).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        let mut values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec![111, 112]);
        Ok(())
    }

    #[test]
    fn test_unary_minus() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(5).
            q(Y) :- p(X) |> let Y = fn:minus(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(-5));
        Ok(())
    }

    #[test]
    fn test_string_concat() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p("hello", "world").
            q(R) :- p(A, B) |> let R = fn:string:concat(A, " ", B).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("hello world".to_string()));
        Ok(())
    }

    #[test]
    fn test_string_replace() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p("foo-bar-baz").
            q(R) :- p(S) |> let R = fn:string:replace(S, "-", "_", -1).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("foo_bar_baz".to_string()));
        Ok(())
    }

    #[test]
    fn test_number_to_string() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(42).
            q(R) :- p(X) |> let R = fn:number:to_string(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("42".to_string()));
        Ok(())
    }

    #[test]
    fn test_float64_to_string() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(3.14).
            q(R) :- p(X) |> let R = fn:float64:to_string(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("3.14".to_string()));
        Ok(())
    }

    #[test]
    fn test_float_promotion_in_sqrt() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(16).
            q(R) :- p(X) |> let R = fn:sqrt(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("q")
            .expect("relation q not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Float(4.0));
        Ok(())
    }

    #[test]
    fn test_negative_number_literals() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            temp(-10).
            temp(5).
            temp(20).
            below_zero(X) :- temp(X), X < 0 .
            offset(X, Y) :- temp(X) |> let Y = fn:float:plus(X, -0.5).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("below_zero")
            .expect("relation below_zero not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(-10));

        let offset_facts: Vec<_> = interpreter
            .store()
            .scan("offset")
            .expect("relation offset not found")
            .collect();
        assert_eq!(offset_facts.len(), 3);

        Ok(())
    }

    #[test]
    fn test_builtin_string_predicates() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            path("/api/users").
            path("/api/posts").
            path("/home").
            path("/api/users/admin").
            api_path(P) :- path(P), :string:starts_with(P, "/api").
            users_path(P) :- path(P), :string:contains(P, "users").
            html_path(P) :- path(P), :string:ends_with(P, "admin").
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let api_facts: Vec<_> = interpreter
            .store()
            .scan("api_path")
            .expect("relation api_path not found")
            .collect();
        assert_eq!(api_facts.len(), 3, "api_path: {:?}", api_facts);

        let users_facts: Vec<_> = interpreter
            .store()
            .scan("users_path")
            .expect("relation users_path not found")
            .collect();
        assert_eq!(users_facts.len(), 2, "users_path: {:?}", users_facts);

        let html_facts: Vec<_> = interpreter
            .store()
            .scan("html_path")
            .expect("relation html_path not found")
            .collect();
        assert_eq!(html_facts.len(), 1, "html_path: {:?}", html_facts);

        Ok(())
    }

    #[test]
    fn test_match_prefix() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            name(/role/admin).
            name(/role).
            name(/other).
            under_role(N) :- name(N), :match_prefix(N, /role).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("under_role")
            .expect("relation under_role not found")
            .collect();
        // "/role" itself should NOT match (must be strictly longer)
        assert_eq!(facts.len(), 1, "under_role: {:?}", facts);
        assert_eq!(facts[0][0], Value::Name("/role/admin".to_string()));

        Ok(())
    }

    #[test]
    fn test_timestamp_literals() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            event(2024-01-15T10:30:00Z).
            event(2024-06-01T00:00:00Z).
            has_event(X) :- event(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("has_event")
            .expect("relation has_event not found")
            .collect();
        assert_eq!(facts.len(), 2);
        // Both should be Time values
        for fact in &facts {
            assert!(matches!(fact[0], Value::Time(_)), "expected Time, got {:?}", fact[0]);
        }
        Ok(())
    }

    #[test]
    fn test_duration_literals() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            timeout(30s).
            timeout(500ms).
            has_timeout(X) :- timeout(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("has_timeout")
            .expect("relation has_timeout not found")
            .collect();
        assert_eq!(facts.len(), 2);
        for fact in &facts {
            assert!(matches!(fact[0], Value::Duration(_)), "expected Duration, got {:?}", fact[0]);
        }
        Ok(())
    }

    #[test]
    fn test_time_arithmetic() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            start(2024-01-15T10:00:00Z).
            result(Y) :- start(X) |> let Y = fn:time:add(X, 1h).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result")
            .expect("relation result not found")
            .collect();
        assert_eq!(facts.len(), 1);
        // 2024-01-15T10:00:00Z + 1h = 2024-01-15T11:00:00Z
        match &facts[0][0] {
            Value::Time(nanos) => {
                let expected = 1705276800_000_000_000i64 + (11 * 3600) * 1_000_000_000;
                assert_eq!(*nanos, expected, "time should be 2024-01-15T11:00:00Z");
            }
            v => panic!("expected Time, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_time_sub() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            pair(2024-01-15T12:00:00Z, 2024-01-15T10:00:00Z).
            diff(D) :- pair(A, B) |> let D = fn:time:sub(A, B).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("diff")
            .expect("relation diff not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Duration(nanos) => {
                assert_eq!(*nanos, 2 * 3600 * 1_000_000_000, "diff should be 2h");
            }
            v => panic!("expected Duration, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_time_components() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            ts(2024-06-15T14:30:45Z).
            year_of(Y) :- ts(T) |> let Y = fn:time:year(T).
            month_of(M) :- ts(T) |> let M = fn:time:month(T).
            day_of(D) :- ts(T) |> let D = fn:time:day(T).
            hour_of(H) :- ts(T) |> let H = fn:time:hour(T).
            minute_of(Min) :- ts(T) |> let Min = fn:time:minute(T).
            second_of(S) :- ts(T) |> let S = fn:time:second(T).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let check = |rel: &str, expected: i64| {
            let facts: Vec<_> = interpreter.store().scan(rel).expect(rel).collect();
            assert_eq!(facts.len(), 1, "{rel}");
            assert_eq!(facts[0][0], Value::Number(expected), "{rel}");
        };
        check("year_of", 2024);
        check("month_of", 6);
        check("day_of", 15);
        check("hour_of", 14);
        check("minute_of", 30);
        check("second_of", 45);
        Ok(())
    }

    #[test]
    fn test_duration_components() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            dur(90s).
            dur_seconds(S) :- dur(D) |> let S = fn:duration:seconds(D).
            dur_nanos(N) :- dur(D) |> let N = fn:duration:nanos(D).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let secs: Vec<_> = interpreter.store().scan("dur_seconds").expect("dur_seconds").collect();
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0][0], Value::Float(90.0));

        let nanos: Vec<_> = interpreter.store().scan("dur_nanos").expect("dur_nanos").collect();
        assert_eq!(nanos.len(), 1);
        assert_eq!(nanos[0][0], Value::Number(90_000_000_000));
        Ok(())
    }

    #[test]
    fn test_time_comparison_predicates() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            event(2024-01-01T00:00:00Z).
            event(2024-06-15T00:00:00Z).
            event(2024-12-31T00:00:00Z).
            recent(T) :- event(T), :time:gt(T, 2024-06-01T00:00:00Z).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("recent")
            .expect("relation recent not found")
            .collect();
        assert_eq!(facts.len(), 2, "recent: {:?}", facts);
        Ok(())
    }

    #[test]
    fn test_date_only_timestamp() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            d(2024-01-15).
            has(X) :- d(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("has")
            .expect("relation has not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Time(nanos) => {
                // 2024-01-15T00:00:00Z
                assert_eq!(*nanos, 1705276800_000_000_000);
            }
            v => panic!("expected Time, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_time_trunc() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            ts(2024-06-15T14:30:45Z).
            truncated(Y) :- ts(T) |> let Y = fn:time:trunc(T, /hour).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("truncated")
            .expect("relation truncated not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Time(nanos) => {
                // Should be truncated to 2024-06-15T14:00:00Z
                let display = format!("{}", Value::Time(*nanos));
                assert_eq!(display, "2024-06-15T14:00:00Z");
            }
            v => panic!("expected Time, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_duration_arithmetic() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            d(1h, 30m).
            total(T) :- d(A, B) |> let T = fn:duration:add(A, B).
            doubled(T) :- d(A, _) |> let T = fn:duration:mult(A, 2).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("total")
            .expect("relation total not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Duration(nanos) => {
                assert_eq!(*nanos, 90 * 60 * 1_000_000_000, "1h + 30m = 90m");
            }
            v => panic!("expected Duration, got {v:?}"),
        }

        let facts: Vec<_> = interpreter
            .store()
            .scan("doubled")
            .expect("relation doubled not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Duration(nanos) => {
                assert_eq!(*nanos, 2 * 3600 * 1_000_000_000, "2 * 1h = 2h");
            }
            v => panic!("expected Duration, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_list_construction() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            data(1). data(2). data(3).
            result(L) :- data(X) |> let L = fn:list(X).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result")
            .expect("relation result not found")
            .collect();
        // Each data fact produces a single-element list
        assert_eq!(facts.len(), 3);
        for fact in &facts {
            match &fact[0] {
                Value::Compound(_, elems) => assert_eq!(elems.len(), 1),
                v => panic!("expected Compound, got {v:?}"),
            }
        }
        Ok(())
    }

    #[test]
    fn test_list_literal_syntax() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // The parser desugars [1, 2, 3] to fn:list(1, 2, 3)
        let source = r#"
            result([1, 2, 3]).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result")
            .expect("relation result not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Compound(_, elems) => {
                assert_eq!(elems.len(), 3);
                assert_eq!(elems[0], Value::Number(1));
                assert_eq!(elems[1], Value::Number(2));
                assert_eq!(elems[2], Value::Number(3));
            }
            v => panic!("expected Compound, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_struct_construction() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            result({/name: "alice", /age: 30}).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result")
            .expect("relation result not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Compound(_, elems) => {
                // Interleaved: ["/name", "alice", "/age", 30]
                assert_eq!(elems.len(), 4);
                assert_eq!(elems[0], Value::Name("/name".to_string()));
                assert_eq!(elems[1], Value::String("alice".to_string()));
                assert_eq!(elems[2], Value::Name("/age".to_string()));
                assert_eq!(elems[3], Value::Number(30));
            }
            v => panic!("expected Compound, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_pair_construction() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            data("key", 42).
            result(P) :- data(K, V) |> let P = fn:pair(K, V).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result")
            .expect("relation result not found")
            .collect();
        assert_eq!(facts.len(), 1);
        match &facts[0][0] {
            Value::Compound(_, elems) => {
                assert_eq!(elems.len(), 2);
                assert_eq!(elems[0], Value::String("key".to_string()));
                assert_eq!(elems[1], Value::Number(42));
            }
            v => panic!("expected Compound, got {v:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_list_get_and_len() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            data([10, 20, 30]).
            first(F) :- data(L) |> let F = fn:list:get(L, 0).
            length(N) :- data(L) |> let N = fn:len(L).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("first")
            .expect("relation first not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(10));

        let facts: Vec<_> = interpreter
            .store()
            .scan("length")
            .expect("relation length not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(3));
        Ok(())
    }

    #[test]
    fn test_struct_get() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            data({/name: "alice", /age: 30}).
            name(N) :- data(S) |> let N = fn:struct:get(S, /name).
            age(A) :- data(S) |> let A = fn:struct:get(S, /age).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("name")
            .expect("relation name not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("alice".to_string()));

        let facts: Vec<_> = interpreter
            .store()
            .scan("age")
            .expect("relation age not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(30));
        Ok(())
    }

    #[test]
    fn test_pair_accessors() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // Use intermediate relation since multiple |> chains not supported
        let source = r#"
            data(42, "hello").
            pair_data(P) :- data(A, B) |> let P = fn:pair(A, B).
            result_first(F) :- pair_data(P) |> let F = fn:pair:first(P).
            result_second(S) :- pair_data(P) |> let S = fn:pair:second(P).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("result_first")
            .expect("relation result_first not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(42));

        let facts: Vec<_> = interpreter
            .store()
            .scan("result_second")
            .expect("relation result_second not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::String("hello".to_string()));
        Ok(())
    }

    #[test]
    fn test_map_operations() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            data([/a: 10, /b: 20]).
            val_a(V) :- data(M) |> let V = fn:map:get(M, /a).
            val_b(V) :- data(M) |> let V = fn:map:get(M, /b).
            nkeys(N) :- data(M) |> let N = fn:map:len(M).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("val_a")
            .expect("relation val_a not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(10));

        let facts: Vec<_> = interpreter
            .store()
            .scan("val_b")
            .expect("relation val_b not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(20));

        let facts: Vec<_> = interpreter
            .store()
            .scan("nkeys")
            .expect("relation nkeys not found")
            .collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0][0], Value::Number(2));
        Ok(())
    }

    #[test]
    fn test_nested_compound() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // A list of pairs
        let source = r#"
            data("a", 1).
            data("b", 2).
            pairs(P) :- data(K, V) |> let P = fn:pair(K, V).
            first_key(K) :- pairs(P) |> let K = fn:pair:first(P).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("first_key")
            .expect("relation first_key not found")
            .collect();
        assert_eq!(facts.len(), 2);
        let mut keys: Vec<String> = facts
            .iter()
            .map(|t| match &t[0] {
                Value::String(s) => s.clone(),
                v => panic!("expected string, got {v:?}"),
            })
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Temporal facts tests
    // -----------------------------------------------------------------------

    /// Non-recursive: temporal facts with point annotations.
    #[test]
    fn test_temporal_point_facts() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            link(/a, /b)@[2024-01-01].
            link(/a, /c)@[2024-01-02].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        // link is temporal so each fact has 4 columns: X, Y, start, end
        let facts: Vec<_> = interpreter
            .store()
            .scan("link")
            .expect("relation link not found")
            .collect();
        assert_eq!(facts.len(), 2, "expected 2 temporal link facts, got {:?}", facts);

        // Each fact should have 4 columns (2 regular + 2 temporal)
        for fact in &facts {
            assert_eq!(fact.len(), 4, "temporal fact should have 4 columns, got {:?}", fact);
        }

        Ok(())
    }

    /// Non-recursive: simple temporal rule copying facts.
    #[test]
    fn test_temporal_simple_rule() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            Decl reachable(X, Y) temporal bound [/name, /name].
            link(/a, /b)@[2024-01-01].
            reachable(X, Y)@[T] :- link(X, Y)@[T].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reachable")
            .expect("relation reachable not found")
            .collect();
        assert_eq!(facts.len(), 1, "expected 1 reachable fact, got {:?}", facts);
        assert_eq!(facts[0].len(), 4); // X, Y, start, end
        assert_eq!(facts[0][0], Value::Name("/a".to_string()));
        assert_eq!(facts[0][1], Value::Name("/b".to_string()));

        Ok(())
    }

    /// Recursive temporal graph reachability with point-in-time intervals.
    /// Based on mangle-go/examples/temporal_graph_points.mg
    #[test]
    fn test_temporal_graph_points() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            Decl reachable(X, Y) temporal bound [/name, /name].

            link(/a, /b)@[2024-01-01].
            link(/b, /c)@[2024-01-01].

            link(/a, /c)@[2024-01-02].
            link(/c, /d)@[2024-01-02].

            reachable(X, Y)@[T] :- link(X, Y)@[T].
            reachable(X, Z)@[T] :- reachable(X, Y)@[T], link(Y, Z)@[T].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;

        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reachable")
            .expect("relation reachable not found")
            .collect();

        let t1: i64 = 1704067200_000_000_000; // 2024-01-01 UTC
        let t2: i64 = 1704153600_000_000_000; // 2024-01-02 UTC

        // Collect as (from, to, time) tuples for easier comparison
        let mut results: Vec<(String, String, i64)> = facts
            .iter()
            .map(|f| {
                let from = match &f[0] {
                    Value::Name(n) => n.clone(),
                    v => panic!("expected name, got {v:?}"),
                };
                let to = match &f[1] {
                    Value::Name(n) => n.clone(),
                    v => panic!("expected name, got {v:?}"),
                };
                let time = match &f[2] {
                    Value::Time(t) => *t,
                    v => panic!("expected time, got {v:?}"),
                };
                (from, to, time)
            })
            .collect();
        results.sort();

        // Expected:
        // T1: a->b, b->c, a->c (derived)
        // T2: a->c, c->d, a->d (derived)
        let expected = vec![
            ("/a".to_string(), "/b".to_string(), t1),
            ("/a".to_string(), "/c".to_string(), t1),
            ("/a".to_string(), "/c".to_string(), t2),
            ("/a".to_string(), "/d".to_string(), t2),
            ("/b".to_string(), "/c".to_string(), t1),
            ("/c".to_string(), "/d".to_string(), t2),
        ];
        assert_eq!(results, expected, "temporal graph reachability mismatch");

        Ok(())
    }

    /// Test temporal interval coalescing: overlapping intervals merge.
    #[test]
    fn test_temporal_coalescing() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // Two overlapping intervals for the same fact should be coalesced
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            Decl reach(X, Y) temporal bound [/name, /name].
            link(/a, /b)@[2024-01-01, 2024-01-05].
            link(/a, /b)@[2024-01-03, 2024-01-10].
            reach(X, Y)@[S, E] :- link(X, Y)@[S, E].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reach")
            .expect("relation reach not found")
            .collect();

        // After coalescing, the two overlapping intervals [Jan 1-5] and [Jan 3-10]
        // should merge into a single [Jan 1, Jan 10] interval.
        assert_eq!(facts.len(), 1, "expected 1 coalesced fact, got {:?}", facts);
        assert_eq!(facts[0][0], Value::Name("/a".to_string()));
        assert_eq!(facts[0][1], Value::Name("/b".to_string()));

        // Verify the merged interval spans Jan 1 to Jan 10
        let start = match &facts[0][2] {
            Value::Time(t) => *t,
            v => panic!("expected time, got {v:?}"),
        };
        let end = match &facts[0][3] {
            Value::Time(t) => *t,
            v => panic!("expected time, got {v:?}"),
        };
        let jan1: i64 = 1704067200_000_000_000;
        let jan10: i64 = 1704844800_000_000_000;
        assert_eq!(start, jan1, "start should be Jan 1");
        assert_eq!(end, jan10, "end should be Jan 10");

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Ported from Go: temporal_integration_test.go
    // -----------------------------------------------------------------------

    /// Go: TestIntegration_TemporalCoalesce - three overlapping intervals coalesce to one
    #[test]
    fn test_temporal_coalesce_three_overlapping() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl status(X) temporal bound [/name].
            status(/active)@[2024-01-01, 2024-01-15].
            status(/active)@[2024-01-10, 2024-01-25].
            status(/active)@[2024-01-20, 2024-01-31].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("status")
            .expect("relation status not found")
            .collect();
        // After coalescing, should be 1 fact spanning Jan 1 to Jan 31
        assert_eq!(facts.len(), 1, "expected 1 coalesced fact, got {:?}", facts);
        assert_eq!(facts[0][0], Value::Name("/active".to_string()));

        Ok(())
    }

    /// Go: TestIntegration_DurationBoundScenarios - temporal recursion reachability
    /// (more complex variant with 3 link facts, some at different timestamps)
    #[test]
    fn test_temporal_reachability_mixed_timestamps() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            Decl reachable(X, Y) temporal bound [/name, /name].

            link(/a, /b)@[2024-01-01].
            link(/b, /c)@[2024-01-01].
            link(/c, /d)@[2024-01-02].

            reachable(X, Y)@[T] :- link(X, Y)@[T].
            reachable(X, Z)@[T] :- reachable(X, Y)@[T], link(Y, Z)@[T].
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reachable")
            .expect("relation reachable not found")
            .collect();

        // Collect as (from, to) ignoring time for uniqueness check
        let mut pairs: Vec<(String, String)> = facts
            .iter()
            .map(|f| {
                let from = match &f[0] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                let to = match &f[1] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                (from, to)
            })
            .collect();
        pairs.sort();
        pairs.dedup();

        // Expected unique pairs (same as Go test):
        // a->b, b->c, a->c (all at t1), c->d (at t2)
        // a->c is derived transitively at t1
        // No a->d because c->d is at t2 but a->c is at t1
        let expected = vec![
            ("/a".to_string(), "/b".to_string()),
            ("/a".to_string(), "/c".to_string()),
            ("/b".to_string(), "/c".to_string()),
            ("/c".to_string(), "/d".to_string()),
        ];
        assert_eq!(pairs, expected, "temporal reachability mismatch");

        Ok(())
    }

    /// Go example: temporal_graph_intervals.mg
    /// Tests interval intersection with 4 cases for deriving reachability.
    #[test]
    fn test_temporal_graph_intervals() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            Decl link(X, Y) temporal bound [/name, /name].
            Decl reachable(X, Y) temporal bound [/name, /name].

            link(/a, /b)@[2024-01-01, 2024-01-10].
            link(/b, /c)@[2024-01-05, 2024-01-15].
            link(/c, /d)@[2024-01-12, 2024-01-20].

            reachable(X, Y)@[S, E] :- link(X, Y)@[S, E].

            reachable(X, Z)@[S1, E1] :-
                reachable(X, Y)@[S1, E1], link(Y, Z)@[S2, E2],
                :time:ge(S1, S2), :time:le(E1, E2), :time:le(S1, E1).

            reachable(X, Z)@[S1, E2] :-
                reachable(X, Y)@[S1, E1], link(Y, Z)@[S2, E2],
                :time:ge(S1, S2), :time:lt(E2, E1), :time:le(S1, E2).

            reachable(X, Z)@[S2, E1] :-
                reachable(X, Y)@[S1, E1], link(Y, Z)@[S2, E2],
                :time:gt(S2, S1), :time:le(E1, E2), :time:le(S2, E1).

            reachable(X, Z)@[S2, E2] :-
                reachable(X, Y)@[S1, E1], link(Y, Z)@[S2, E2],
                :time:gt(S2, S1), :time:lt(E2, E1), :time:le(S2, E2).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("reachable")
            .expect("relation reachable not found")
            .collect();

        // Collect unique (from, to) pairs
        let mut pairs: Vec<(String, String)> = facts
            .iter()
            .map(|f| {
                let from = match &f[0] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                let to = match &f[1] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                (from, to)
            })
            .collect();
        pairs.sort();
        pairs.dedup();

        // Expected (from the Go example comments):
        // a->b, b->c, c->d (direct)
        // a->c (intersection of [Jan1-10] and [Jan5-15] = [Jan5-10])
        // b->d (intersection of [Jan5-15] and [Jan12-20] = [Jan12-15])
        // a->d NOT derived (a->c is [Jan5-10], c->d is [Jan12-20] — no overlap)
        let expected = vec![
            ("/a".to_string(), "/b".to_string()),
            ("/a".to_string(), "/c".to_string()),
            ("/b".to_string(), "/c".to_string()),
            ("/b".to_string(), "/d".to_string()),
            ("/c".to_string(), "/d".to_string()),
        ];
        assert_eq!(pairs, expected, "interval intersection reachability mismatch");

        // Verify specific intervals for derived facts
        for f in &facts {
            let from = match &f[0] { Value::Name(s) => s.as_str(), _ => "" };
            let to = match &f[1] { Value::Name(s) => s.as_str(), _ => "" };
            let start = match &f[2] { Value::Time(t) => *t, _ => 0 };
            let end = match &f[3] { Value::Time(t) => *t, _ => 0 };

            if from == "/a" && to == "/c" {
                // Intersection of [Jan1,Jan10] and [Jan5,Jan15] = [Jan5,Jan10]
                let jan5: i64 = 1704412800_000_000_000;
                let jan10: i64 = 1704844800_000_000_000;
                assert_eq!(start, jan5, "a->c start should be Jan 5");
                assert_eq!(end, jan10, "a->c end should be Jan 10");
            }
            if from == "/b" && to == "/d" {
                // Intersection of [Jan5,Jan15] and [Jan12,Jan20] = [Jan12,Jan15]
                let jan12: i64 = 1705017600_000_000_000;
                let jan15: i64 = 1705276800_000_000_000;
                assert_eq!(start, jan12, "b->d start should be Jan 12");
                assert_eq!(end, jan15, "b->d end should be Jan 15");
            }
        }

        Ok(())
    }

    /// Go example: temporal_sequence.mg (adapted for mangle-rs syntax)
    /// Event sequence detection: A followed by B within 10 minutes.
    /// Uses fn:time:sub to compute duration and :duration:le for comparison.
    /// Split into two rules since mangle-rs doesn't support inline assignments in body.
    #[test]
    fn test_temporal_sequence_detection() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        // Strategy: compute the time difference via transform, then filter.
        // Rule 1: compute (U, Diff) pairs where B happens after A
        // Rule 2: filter where Diff <= 10 minutes (600 seconds = 600_000_000_000 ns)
        let source = r#"
            Decl event_a(Name) temporal bound [/name].
            Decl event_b(Name) temporal bound [/name].

            event_a(/u1)@[2024-01-01T10:00:00Z].
            event_b(/u1)@[2024-01-01T10:05:00Z].

            event_a(/u2)@[2024-01-01T10:00:00Z].
            event_b(/u2)@[2024-01-01T10:15:00Z].

            seq_pair(U, Diff) :-
                event_b(U)@[Tb],
                event_a(U)@[Ta],
                :time:lt(Ta, Tb)
                |> let Diff = fn:time:sub(Tb, Ta).

            match_seq(U) :-
                seq_pair(U, Diff),
                :duration:le(Diff, 600000000000).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("match_seq")
            .expect("relation match_seq not found")
            .collect();

        // Only /u1 should match (5 min gap = 300s), /u2 has 15 min gap = 900s
        assert_eq!(facts.len(), 1, "expected 1 match, got {:?}", facts);
        assert_eq!(facts[0][0], Value::Name("/u1".to_string()));

        Ok(())
    }

    /// Test: non-temporal programs are unaffected by temporal machinery
    #[test]
    fn test_temporal_backward_compat_reachability() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            edge(/a, /b).
            edge(/b, /c).
            edge(/c, /d).
            path(X, Y) :- edge(X, Y).
            path(X, Z) :- edge(X, Y), path(Y, Z).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("path")
            .expect("relation path not found")
            .collect();

        let mut pairs: Vec<(String, String)> = facts
            .iter()
            .map(|f| {
                let from = match &f[0] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                let to = match &f[1] { Value::Name(s) => s.clone(), v => panic!("{v:?}") };
                (from, to)
            })
            .collect();
        pairs.sort();

        let expected = vec![
            ("/a".to_string(), "/b".to_string()),
            ("/a".to_string(), "/c".to_string()),
            ("/a".to_string(), "/d".to_string()),
            ("/b".to_string(), "/c".to_string()),
            ("/b".to_string(), "/d".to_string()),
            ("/c".to_string(), "/d".to_string()),
        ];
        assert_eq!(pairs, expected);

        Ok(())
    }

    /// `X = expr` in a rule body with a fresh LHS variable is a let-binding,
    /// equivalent to `|> let X = expr`. Matches mangle-go semantics.
    #[test]
    fn test_body_eq_as_let_binding() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1). p(2). p(3).
            q(Y) :- p(X), Y = fn:plus(X, 10).
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;
        let mut got: Vec<i64> = interpreter
            .store()
            .scan("q")?
            .map(|f| match &f[0] { Value::Number(n) => *n, v => panic!("{v:?}") })
            .collect();
        got.sort();
        assert_eq!(got, vec![11, 12, 13]);
        Ok(())
    }

    /// When both sides of `=` are bound, it remains a runtime equality check
    /// (non-matching rows are filtered out) rather than becoming a binding.
    #[test]
    fn test_body_eq_still_filters_when_bound() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1). p(2). p(3).
            only_two(X) :- p(X), X = 2.
        "#;
        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;
        let got: Vec<i64> = interpreter
            .store()
            .scan("only_two")?
            .map(|f| match &f[0] { Value::Number(n) => *n, v => panic!("{v:?}") })
            .collect();
        assert_eq!(got, vec![2]);
        Ok(())
    }

    /// Test: negation with temporal still works
    #[test]
    fn test_temporal_backward_compat_negation() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            all(/a). all(/b). all(/c).
            excluded(/a).
            included(X) :- all(X), !excluded(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = execute(&mut ir, &stratified, store)?;

        let facts: Vec<_> = interpreter
            .store()
            .scan("included")
            .expect("relation included not found")
            .collect();

        let mut values: Vec<String> = facts
            .iter()
            .map(|f| match &f[0] { Value::Name(s) => s.clone(), v => panic!("{v:?}") })
            .collect();
        values.sort();
        assert_eq!(values, vec!["/b", "/c"]);

        Ok(())
    }
}
