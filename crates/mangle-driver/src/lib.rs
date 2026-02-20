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
use mangle_analysis::{LoweringContext, Planner, Program, StratifiedProgram, rewrite_unit};
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
            if let ast::Term::Atom(atom) = premise {
                all_preds.insert(atom.sym);
            } else if let ast::Term::NegAtom(atom) = premise {
                all_preds.insert(atom.sym);
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
    let ir = ctx.lower_unit(unit);

    Ok((ir, stratified))
}

/// Compiles the Intermediate Representation (IR) into a WebAssembly (WASM) module.
///
/// This uses the default `WasmImportsBackend` which expects certain host functions
/// to be available for data access.
pub fn compile_to_wasm(ir: &mut Ir, stratified: &StratifiedProgram) -> Vec<u8> {
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

    // 2. Now execute using the interpreter
    let mut interpreter = Interpreter::new(ir, store);

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
            }
            None => {}
        }
        interpreter.store_mut().merge_deltas();
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
                Value::String(s) => s.clone(),
                _ => panic!("expected string"),
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
    fn test_compile_to_wasm() -> Result<()> {
        let arena = Arena::new_with_global_interner();
        let source = r#"
            p(1).
            q(X) :- p(X).
        "#;

        let (mut ir, stratified) = compile(source, &arena)?;
        let wasm_bytes = compile_to_wasm(&mut ir, &stratified);

        // Basic check that we generated something that looks like WASM
        assert!(!wasm_bytes.is_empty());
        assert_eq!(&wasm_bytes[0..4], b"\0asm"); // WASM magic header

        Ok(())
    }
}
