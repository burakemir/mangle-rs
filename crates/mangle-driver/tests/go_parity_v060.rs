// Reproductions of mangle-go v0.6.0 correctness fixes, checked against the
// mangle-rs driver pipeline (parse -> analysis -> plan -> interpreter).

use mangle_ast::Arena;
use mangle_driver::{compile, execute};
use mangle_interpreter::{MemStore, Value};

fn run(source: &str) -> anyhow::Result<Vec<String>> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = compile(source, &arena)?;
    let store = Box::new(MemStore::new());
    let interpreter = execute(&mut ir, &stratified, store)?;
    let mut out = Vec::new();
    let mut names = interpreter.store().relation_names();
    names.sort();
    for name in names {
        if name.starts_with('$') {
            continue;
        }
        for tuple in interpreter.store().scan(&name)? {
            out.push(format!(
                "{}({})",
                name,
                tuple
                    .iter()
                    .map(|v| match v {
                        Value::Number(n) => n.to_string(),
                        Value::Float(f) => f.to_string(),
                        Value::String(s) => format!("{s:?}"),
                        Value::Name(s) => format!("/{s}"),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    out.sort();
    Ok(out)
}

fn numbers(rel: &str, facts: &[String]) -> Vec<i64> {
    facts
        .iter()
        .filter(|f| f.starts_with(&format!("{rel}(")))
        .map(|f| f[rel.len() + 1..f.len() - 1].parse::<i64>().unwrap())
        .collect()
}

// --- Go #98: repeated variable in a premise must match only equal arguments ---
#[test]
fn repeated_var_in_premise() -> anyhow::Result<()> {
    let facts = run("e(1, 2). e(3, 3). q(X) :- e(X, X).")?;
    println!("repeated_var_in_premise: {facts:?}");
    let mut qs = numbers("q", &facts);
    qs.sort();
    assert_eq!(qs, vec![3], "e(X, X) must only match e(3, 3)");
    Ok(())
}

// --- Go #98 (aggregation variant, like the Go regression test but chosen so
// the wrong matches do not coincide with the right ones) ---
#[test]
fn repeated_var_in_aggregation_premise() -> anyhow::Result<()> {
    let facts = run("e(1, 2). e(3, 3). e(4, 5).\n\
         n(N) :- e(X, X) |> do fn:group_by(), let N = fn:count(X).")?;
    println!("repeated_var_in_aggregation_premise: {facts:?}");
    let ns = numbers("n", &facts);
    assert_eq!(ns, vec![1], "only e(3, 3) has equal arguments");
    Ok(())
}

// --- Go #100: two premises of a recursive rule that first hold in the same
// semi-naive round must still be joined ---
#[test]
fn join_of_facts_new_in_same_round() -> anyhow::Result<()> {
    let facts = run("start(1).\n\
         next(1, 2).\n\
         s(X) :- start(X).\n\
         l(X) :- s(X).\n\
         r(X) :- s(X).\n\
         s(Y) :- l(X), r(X), next(X, Y).")?;
    println!("join_of_facts_new_in_same_round: {facts:?}");
    let mut ss = numbers("s", &facts);
    ss.sort();
    assert_eq!(ss, vec![1, 2], "s(2) requires joining l(1) and r(1)");
    Ok(())
}

// --- Go #99: inequality before its variables are bound ---
#[test]
fn inequality_before_binding() -> anyhow::Result<()> {
    let result = run("e(1). e(2). p(X) :- X != 1, e(X).");
    match result {
        Ok(facts) => {
            println!("inequality_before_binding: {facts:?}");
            let mut ps = numbers("p", &facts);
            ps.sort();
            assert_eq!(ps, vec![2]);
        }
        Err(e) => panic!("program should evaluate (Go delays the inequality): {e}"),
    }
    Ok(())
}

// --- Go PR #96 / 6a10591: negation before its variables are bound ---
#[test]
fn negation_before_binding() -> anyhow::Result<()> {
    let result = run("q(1). q(2). e(2). p(X) :- !e(X), q(X).");
    match result {
        Ok(facts) => {
            println!("negation_before_binding: {facts:?}");
            let mut ps = numbers("p", &facts);
            ps.sort();
            assert_eq!(ps, vec![1]);
        }
        Err(e) => panic!("program should evaluate (Go delays the negation): {e}"),
    }
    Ok(())
}

// --- Go 6a10591: negation whose variable is never bound must be an error,
// not silently dropped ---
#[test]
fn negation_with_never_bound_variable_is_error() -> anyhow::Result<()> {
    let result = run("q(1). e(9). p(W) :- q(W), !e(_, W).");
    // `_` is a wildcard; !e(_, W) with W bound by q(W) is fine and should
    // evaluate. Use a truly-never-bound variable instead:
    if result.is_ok() {
        println!("negation_with_never_bound_variable_is_error (wildcard): {result:?}");
    }
    let result = run("q(1). p(W) :- q(W), !e(Z, W).");
    assert!(
        result.is_err(),
        "!e(Z, W) with Z never bound must be an analysis error, not a silent drop"
    );
    Ok(())
}

// --- Go PR #96 case (2): two delayed negations must both survive the
// rewrite (the old Go code removed by position and lost one) ---
#[test]
fn two_delayed_negations_both_kept() -> anyhow::Result<()> {
    let facts = run("q(1). r(5). a(9). b(1).
         p(X) :- !a(Y), !b(X), q(X), r(Y).")?
    .into_iter()
    .filter(|f| f.starts_with("p("))
    .collect::<Vec<_>>();
    println!("two_delayed_negations_both_kept: {facts:?}");
    // X = 1 (from q), Y = 5 (from r); b(1) exists so !b(X) fails: p must be empty.
    assert!(facts.is_empty(), "!b(X) must still be evaluated");

    // Same shape, but now both negations succeed.
    let facts = run("q(1). r(5). a(9). b(2).
         p(X) :- !a(Y), !b(X), q(X), r(Y).")?
    .into_iter()
    .filter(|f| f.starts_with("p("))
    .collect::<Vec<_>>();
    assert_eq!(facts, vec!["p(1)".to_string()]);
    Ok(())
}

// --- A negated built-in predicate before its variable is bound is delayed
// like a plain negation (previously it errored at plan time). ---
#[test]
fn negated_builtin_before_binding() -> anyhow::Result<()> {
    let facts = run("q(1). q(2). p(X) :- !:list:member(X, [1]), q(X).")?;
    println!("negated_builtin_before_binding: {facts:?}");
    let mut ps = numbers("p", &facts);
    ps.sort();
    assert_eq!(ps, vec![2]);
    Ok(())
}

// --- Equality whose other side is bound later. mangle-go unifies lazily,
// so `X = Y, e(Y)` is legal there; the planner now delays the equality
// until its right-hand side is bound and plans it as a let-binding. ---
#[test]
fn equality_before_binding() -> anyhow::Result<()> {
    let facts = run("e(1). e(2). p(X) :- X = Y, e(Y).")?;
    println!("equality_before_binding: {facts:?}");
    let mut ps = numbers("p", &facts);
    ps.sort();
    assert_eq!(ps, vec![1, 2]);
    Ok(())
}

// --- Chained equalities with forward references resolve via the
// waiting-list fixpoint: `X = Y, Y = Z, e(Z)`. ---
#[test]
fn chained_equalities_before_binding() -> anyhow::Result<()> {
    let facts = run("e(5). p(X) :- X = Y, Y = Z, e(Z).")?;
    println!("chained_equalities_before_binding: {facts:?}");
    let ps = numbers("p", &facts);
    assert_eq!(ps, vec![5]);
    Ok(())
}

// --- `X = expr` where expr's variables are bound later. mangle-go rejects
// this (apply-expression variables must be bound at the premise); we
// deliberately accept it — the delayed equality becomes a let-binding once
// the expression can be evaluated. ---
#[test]
fn equality_with_expression_before_binding() -> anyhow::Result<()> {
    let facts = run("e(1). e(2). p(X) :- X = fn:plus(Y, 1), e(Y).")?;
    println!("equality_with_expression_before_binding: {facts:?}");
    let mut ps = numbers("p", &facts);
    ps.sort();
    assert_eq!(ps, vec![2, 3]);
    Ok(())
}

// --- A negation waiting for a variable that only a delayed equality binds:
// `!q(X)` must be placed after `X = Y` (which itself waits for `e(Y)`). ---
#[test]
fn negation_waiting_for_equality_bound_variable() -> anyhow::Result<()> {
    let facts = run("e(1). q(2). p(X) :- !q(X), X = Y, e(Y).")?;
    println!("negation_waiting_for_equality_bound_variable: {facts:?}");
    let ps = numbers("p", &facts);
    assert_eq!(ps, vec![1], "!q(1) must succeed (only q(2) exists)");

    // Same program with q(1): the negation must now fail.
    let facts = run("e(1). q(1). p(X) :- !q(X), X = Y, e(Y).")?;
    let ps = numbers("p", &facts);
    assert!(ps.is_empty());
    Ok(())
}

// --- An equality whose variables nothing binds stays an error (the
// premise is appended, not dropped). ---
#[test]
fn equality_with_never_bound_variable_is_error() -> anyhow::Result<()> {
    let result = run("q(1). p(X) :- q(W), X = Y.");
    assert!(
        result.is_err(),
        "X = Y with neither variable bound must be an analysis error"
    );
    Ok(())
}
