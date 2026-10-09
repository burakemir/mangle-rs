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

use crate::{LoweringContext, TypeChecker};
use mangle_ast as ast;
use mangle_ir::Inst;

#[test]
fn test_lowering_and_type_check_basic() {
    let arena = ast::Arena::new_with_global_interner();

    // Decl foo(X) bound [/number].
    let foo_sym = arena.predicate_sym("foo", Some(1));
    let var_x = arena.variable("X");
    let atom_foo_x = arena.atom(foo_sym, &[var_x]);

    let num_type = arena.const_(arena.name("/number"));
    let bound_decl = ast::BoundDecl {
        base_terms: arena.alloc_slice_copy(&[num_type]),
    };
    let bound_ref = arena.alloc(bound_decl);

    let decl = ast::Decl {
        atom: atom_foo_x,
        descr: &[],
        bounds: Some(arena.alloc_slice_copy(&[bound_ref])),
        constraints: None,
        is_temporal: false,
    };

    // foo(42).
    let const_42 = arena.const_(ast::Const::Number(42));
    let atom_foo_42 = arena.atom(foo_sym, &[const_42]);
    let clause = ast::Clause {
        head: atom_foo_42,
        head_time: None,
        premises: &[],
        transform: &[],
    };

    let unit = ast::Unit {
        decls: arena.alloc_slice_copy(&[&decl]),
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let ir = ctx.lower_unit(&unit);

    // Verify IR contains Decl and Rule
    let has_decl = ir.insts.iter().any(|i| matches!(i, Inst::Decl { .. }));
    let has_rule = ir.insts.iter().any(|i| matches!(i, Inst::Rule { .. }));
    assert!(has_decl, "IR missing Decl");
    assert!(has_rule, "IR missing Rule");

    // Type Check
    let mut checker = TypeChecker::new(&ir);
    assert!(
        checker.check().is_ok(),
        "Type check failed for valid program"
    );
}

#[test]
fn test_type_check_arity_ismatch() {
    let arena = ast::Arena::new_with_global_interner();

    // Decl foo(X) bound [/number].
    let foo_sym = arena.predicate_sym("foo", Some(1));
    let var_x = arena.variable("X");
    let atom_foo_x = arena.atom(foo_sym, &[var_x]);

    let num_type = arena.const_(arena.name("/number"));
    let bound_decl = ast::BoundDecl {
        base_terms: arena.alloc_slice_copy(&[num_type]),
    };
    let bound_ref = arena.alloc(bound_decl);

    let decl = ast::Decl {
        atom: atom_foo_x,
        descr: &[],
        bounds: Some(arena.alloc_slice_copy(&[bound_ref])),
        constraints: None,
        is_temporal: false,
    };

    // foo(42, 43). -> Arity mismatch (defined as 1, used as 2)
    let const_42 = arena.const_(ast::Const::Number(42));
    let const_43 = arena.const_(ast::Const::Number(43));
    let atom_foo_bad = arena.atom(foo_sym, &[const_42, const_43]); // AST allows this construction
    let clause = ast::Clause {
        head: atom_foo_bad,
        head_time: None,
        premises: &[],
        transform: &[],
    };

    let unit = ast::Unit {
        decls: arena.alloc_slice_copy(&[&decl]),
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let ir = ctx.lower_unit(&unit);

    let mut checker = TypeChecker::new(&ir);
    let result = checker.check();
    assert!(result.is_err());
    let err = result.err().unwrap().to_string();
    assert!(err.contains("Arity mismatch"), "Unexpected error: {}", err);
}

#[test]
fn test_type_check_type_mismatch() {
    let arena = ast::Arena::new_with_global_interner();

    // Decl foo(X) bound [/number].
    let foo_sym = arena.predicate_sym("foo", Some(1));
    let var_x = arena.variable("X");
    let atom_foo_x = arena.atom(foo_sym, &[var_x]);

    let num_type = arena.const_(arena.name("/number"));
    let bound_decl = ast::BoundDecl {
        base_terms: arena.alloc_slice_copy(&[num_type]),
    };
    let bound_ref = arena.alloc(bound_decl);

    let decl = ast::Decl {
        atom: atom_foo_x,
        descr: &[],
        bounds: Some(arena.alloc_slice_copy(&[bound_ref])),
        constraints: None,
        is_temporal: false,
    };

    // foo("string"). -> Type mismatch
    let const_string = arena.const_(ast::Const::String("hello"));
    let atom_foo_bad = arena.atom(foo_sym, &[const_string]);
    let clause = ast::Clause {
        head: atom_foo_bad,
        head_time: None,
        premises: &[],
        transform: &[],
    };

    let unit = ast::Unit {
        decls: arena.alloc_slice_copy(&[&decl]),
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let ir = ctx.lower_unit(&unit);

    let mut checker = TypeChecker::new(&ir);
    let result = checker.check();
    assert!(result.is_err());
    let err = result.err().unwrap().to_string();
    assert!(err.contains("Type mismatch"), "Unexpected error: {}", err);
}

#[test]
fn test_planner_basic() {
    let arena = ast::Arena::new_with_global_interner();
    // Rule: p(X) :- q(X).
    let p = arena.predicate_sym("p", Some(1));
    let q = arena.predicate_sym("q", Some(1));
    let x = arena.variable("X");

    let head = arena.atom(p, &[x]);
    let premise = arena.atom(q, &[x]);

    let clause = ast::Clause {
        head,
        head_time: None,
        premises: arena.alloc_slice_copy(&[arena.alloc(ast::Term::Atom(premise))]),
        transform: &[],
    };

    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);

    // Find rule
    let rule_id = ir
        .insts
        .iter()
        .position(|i| matches!(i, Inst::Rule { .. }))
        .unwrap();
    let rule_inst = mangle_ir::InstId::new(rule_id);

    use crate::Planner;
    let planner = Planner::new(&mut ir);
    let op = planner.plan_rule(rule_inst).unwrap();

    // Check if Op is Iterate -> Insert
    use mangle_ir::physical::Op;
    if let Op::Iterate { body, .. } = op {
        if let Op::Insert { relation, .. } = *body {
            assert_eq!(ir.resolve_name(relation), "p");
        } else {
            panic!("Expected inner Insert");
        }
    } else {
        panic!("Expected outer Iterate");
    }
}

/// Setting `MANGLE_HASHJOIN=1` must cause the planner to emit `Op::HashJoin`
/// for a two-premise rule whose shared variable is unbound on both sides.
///
/// This test is the counterpart to the hand-constructed HashJoin tests in
/// mangle-interpreter: those prove execution is correct; this proves the
/// planner wires it up.
#[test]
fn test_planner_emits_hash_join_under_env_var() {
    // `result(X, Y) :- a(X, Z), b(Z, Y).`
    let arena = ast::Arena::new_with_global_interner();
    let result_pred = arena.predicate_sym("result", Some(2));
    let a = arena.predicate_sym("a", Some(2));
    let b = arena.predicate_sym("b", Some(2));
    let var_x = arena.variable("X");
    let var_y = arena.variable("Y");
    let var_z = arena.variable("Z");

    let head = arena.atom(result_pred, &[var_x, var_y]);
    let prem_a = arena.atom(a, &[var_x, var_z]);
    let prem_b = arena.atom(b, &[var_z, var_y]);

    let clause = ast::Clause {
        head,
        head_time: None,
        premises: arena.alloc_slice_copy(&[
            arena.alloc(ast::Term::Atom(prem_a)),
            arena.alloc(ast::Term::Atom(prem_b)),
        ]),
        transform: &[],
    };
    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&clause]),
    };

    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);
    let rule_id = ir
        .insts
        .iter()
        .position(|i| matches!(i, Inst::Rule { .. }))
        .unwrap();
    let rule_inst = mangle_ir::InstId::new(rule_id);

    use crate::Planner;
    use mangle_ir::physical::{DataSource, Op};

    let op = Planner::new(&mut ir)
        .with_hash_join(true)
        .plan_rule(rule_inst)
        .unwrap();

    // Top-level op must be HashJoin; body must be Insert into `result`.
    let Op::HashJoin {
        build_source,
        probe_source,
        join_keys,
        body,
    } = op
    else {
        panic!("expected HashJoin at top level, got: {op:?}");
    };

    assert_eq!(join_keys.len(), 1, "expected single shared join key (Z)");
    match build_source {
        DataSource::Scan { relation, vars } => {
            assert_eq!(ir.resolve_name(relation), "a");
            assert_eq!(vars.len(), 2);
        }
        other => panic!("build_source: expected Scan(a), got {other:?}"),
    }
    match probe_source {
        DataSource::Scan { relation, vars } => {
            assert_eq!(ir.resolve_name(relation), "b");
            assert_eq!(vars.len(), 2);
        }
        other => panic!("probe_source: expected Scan(b), got {other:?}"),
    }
    match *body {
        Op::Insert { relation, .. } => assert_eq!(ir.resolve_name(relation), "result"),
        other => panic!("body: expected Insert into result, got {other:?}"),
    }
}

/// Without the env var, the planner must continue to emit the classic nested
/// `Op::Iterate` structure — guarding against accidental HashJoin regressions
/// for consumers (like the WASM codegen path) that don't support it yet.
#[test]
fn test_planner_no_hash_join_by_default() {
    let arena = ast::Arena::new_with_global_interner();
    let result_pred = arena.predicate_sym("result", Some(2));
    let a = arena.predicate_sym("a", Some(2));
    let b = arena.predicate_sym("b", Some(2));
    let var_x = arena.variable("X");
    let var_y = arena.variable("Y");
    let var_z = arena.variable("Z");

    let head = arena.atom(result_pred, &[var_x, var_y]);
    let prem_a = arena.atom(a, &[var_x, var_z]);
    let prem_b = arena.atom(b, &[var_z, var_y]);

    let clause = ast::Clause {
        head,
        head_time: None,
        premises: arena.alloc_slice_copy(&[
            arena.alloc(ast::Term::Atom(prem_a)),
            arena.alloc(ast::Term::Atom(prem_b)),
        ]),
        transform: &[],
    };
    let unit = ast::Unit {
        decls: &[],
        clauses: arena.alloc_slice_copy(&[&clause]),
    };
    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(&unit);
    let rule_id = ir
        .insts
        .iter()
        .position(|i| matches!(i, Inst::Rule { .. }))
        .unwrap();
    let rule_inst = mangle_ir::InstId::new(rule_id);

    use crate::Planner;
    use mangle_ir::physical::Op;

    // Explicitly disable to defeat any env-var leakage from another test.
    let op = Planner::new(&mut ir)
        .with_hash_join(false)
        .plan_rule(rule_inst)
        .unwrap();
    assert!(
        !matches!(op, Op::HashJoin { .. }),
        "HashJoin must not be emitted when disabled"
    );
}

/// Helper: parse source, lower, plan the first rule.
fn plan_first_rule(source: &str) -> (mangle_ir::Ir, mangle_ir::physical::Op) {
    let arena = ast::Arena::new_with_global_interner();
    let mut parser = mangle_parse::Parser::new(&arena, source.as_bytes(), "test");
    parser.next_token().unwrap();
    let unit = parser.parse_unit().unwrap();
    let ctx = LoweringContext::new(&arena);
    let mut ir = ctx.lower_unit(unit);
    let rule_id = ir
        .insts
        .iter()
        .position(|i| matches!(i, Inst::Rule { .. }))
        .unwrap();
    let rule_inst = mangle_ir::InstId::new(rule_id);
    use crate::Planner;
    let op = Planner::new(&mut ir)
        .with_hash_join(false)
        .plan_rule(rule_inst)
        .unwrap();
    (ir, op)
}

/// A variable that repeats within one premise atom must only match facts with
/// equal arguments at those positions: the second occurrence is scanned into
/// a fresh variable with an equality constraint (mangle-go #98).
#[test]
fn test_planner_repeated_variable_gets_equality_constraint() {
    use mangle_ir::physical::{DataSource, Op};
    let (ir, op) = plan_first_rule("q(X) :- e(X, X).");
    let Op::Iterate {
        source: DataSource::Scan { vars, .. },
        body,
    } = op
    else {
        panic!("expected Iterate over e, got: {op:?}");
    };
    // The two occurrences must not be the same scan variable.
    assert_ne!(
        vars[0], vars[1],
        "repeated variable must get a fresh scan var"
    );
    // ... and the body must enforce their equality before inserting.
    let Op::Filter { .. } = *body else {
        panic!("expected equality filter under the scan, got: {body:?}");
    };
    assert_eq!(ir.resolve_name(vars[0]), "X");
}

/// Negations and inequalities whose variables are not yet bound are delayed
/// until a premise binds them, so premise order does not matter (mangle-go
/// PRs #96 and #99): `p(X) :- X != 1, e(X).` plans the scan of `e` first and
/// the `!=` filter underneath it.
#[test]
fn test_planner_delays_inequality_until_bound() {
    use mangle_ir::physical::{CmpOp, Condition, DataSource, Op};
    let (ir, op) = plan_first_rule("p(X) :- X != 1, e(X).");
    let Op::Iterate {
        source: DataSource::Scan { relation, .. },
        body,
    } = op
    else {
        panic!("expected Iterate (scan must come first), got: {op:?}");
    };
    assert_eq!(ir.resolve_name(relation), "e");
    let Op::Filter {
        cond: Condition::Cmp { op: cmp, .. },
        ..
    } = *body
    else {
        panic!("expected != filter under the scan, got: {body:?}");
    };
    assert_eq!(cmp, CmpOp::Neq);
}

/// A negation whose variables a later premise binds is delayed past it, and
/// still-waiting negations are appended rather than dropped (mangle-go
/// PR #96).
#[test]
fn test_planner_delays_negation_until_bound() {
    use mangle_ir::physical::{Condition, DataSource, Op};
    let (_, op) = plan_first_rule("p(X) :- !e(X), q(X).");
    // The scan of q must come first; the negation filter sits underneath it.
    let Op::Iterate {
        source: DataSource::Scan { .. },
        body,
    } = op
    else {
        panic!("expected Iterate (scan must come first), got: {op:?}");
    };
    let Op::Filter {
        cond: Condition::Negation { .. },
        ..
    } = *body
    else {
        panic!("expected negation filter under the scan, got: {body:?}");
    };
}

/// An equality whose other side is bound later is delayed and planned as a
/// let-binding underneath the scan (mangle-go unifies variable-variable
/// equalities lazily; the reorder achieves the same for plans).
#[test]
fn test_planner_delays_equality_until_bound() {
    use mangle_ir::physical::{DataSource, Expr, Op};
    let (ir, op) = plan_first_rule("p(X) :- X = Y, e(Y).");
    let Op::Iterate {
        source: DataSource::Scan { relation, vars },
        body,
    } = op
    else {
        panic!("expected Iterate (scan must come first), got: {op:?}");
    };
    assert_eq!(ir.resolve_name(relation), "e");
    assert_eq!(vars.len(), 1, "only Y is a scan variable");
    let Op::Let {
        var,
        expr: Expr::Value(operand),
        body,
    } = *body
    else {
        panic!("expected Let (X = Y) under the scan, got: {body:?}");
    };
    assert_eq!(ir.resolve_name(var), "X");
    let _ = operand;
    assert!(
        matches!(*body, Op::Insert { .. }),
        "expected Insert under the Let, got: {body:?}"
    );
}

/// A negation whose variable is bound only by a delayed equality is placed
/// after that equality (the waiting list drains to a fixpoint).
#[test]
fn test_planner_delays_negation_behind_equality() {
    use mangle_ir::physical::{Condition, DataSource, Op};
    let (_, op) = plan_first_rule("p(X) :- !q(X), X = Y, e(Y).");
    // e(Y) scan first...
    let Op::Iterate {
        source: DataSource::Scan { .. },
        body,
    } = op
    else {
        panic!("expected Iterate (scan must come first), got: {op:?}");
    };
    // ... then Let X = Y...
    let Op::Let { body, .. } = *body else {
        panic!("expected Let (X = Y) under the scan, got: {body:?}");
    };
    // ... then the negation filter.
    let Op::Filter {
        cond: Condition::Negation { .. },
        ..
    } = *body
    else {
        panic!("expected negation filter after the Let, got: {body:?}");
    };
}
