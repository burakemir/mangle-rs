// Each GroupBy rule materializes its body into a temp relation before
// grouping. These check that two GroupBy rules never share that relation,
// whether they sit in the same stratum or in different ones.

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
                        Value::String(s) => format!("{s:?}"),
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

fn rows(rel: &str, facts: &[String]) -> Vec<String> {
    facts
        .iter()
        .filter(|f| f.starts_with(&format!("{rel}(")))
        .cloned()
        .collect()
}

// Same number of captured variables: rows from one rule's body leaked into
// the other's groups.
#[test]
fn two_groupby_rules_over_different_relations() -> anyhow::Result<()> {
    let facts = run(r#"
        a("x", 1). a("x", 2).
        b("x", 10). b("y", 20).
        cnt_a(C, N) :- a(C, V) |> do fn:group_by(C), let N = fn:count(V).
        sum_b(C, N) :- b(C, V) |> do fn:group_by(C), let N = fn:sum(V).
    "#)?;
    assert_eq!(rows("cnt_a", &facts), vec![r#"cnt_a("x", 2)"#]);
    assert_eq!(
        rows("sum_b", &facts),
        vec![r#"sum_b("x", 10)"#, r#"sum_b("y", 20)"#]
    );
    Ok(())
}

// Different number of captured variables: the shared relation took the
// arity of whichever rule ran first and dropped the other rule's rows.
#[test]
fn two_groupby_rules_with_different_captured_arity() -> anyhow::Result<()> {
    let facts = run(r#"
        a("x", 1). a("x", 2).
        b("x", 10, "w"). b("y", 20, "w").
        cnt_a(C, N) :- a(C, V) |> do fn:group_by(C), let N = fn:count(V).
        sum_b(C, N) :- b(C, V, W) |> do fn:group_by(C), let N = fn:sum(V).
    "#)?;
    assert_eq!(rows("cnt_a", &facts), vec![r#"cnt_a("x", 2)"#]);
    assert_eq!(
        rows("sum_b", &facts),
        vec![r#"sum_b("x", 10)"#, r#"sum_b("y", 20)"#]
    );
    Ok(())
}

// The second rule reads the first rule's output, so it lands in a later
// stratum. The first rule's temp rows must not reach it.
#[test]
fn groupby_rules_in_different_strata() -> anyhow::Result<()> {
    let facts = run(r#"
        a("x", 1). a("x", 2).
        cnt_a(C, N) :- a(C, V) |> do fn:group_by(C), let N = fn:count(V).
        sum_n(C, S) :- cnt_a(C, N) |> do fn:group_by(C), let S = fn:sum(N).
    "#)?;
    assert_eq!(rows("cnt_a", &facts), vec![r#"cnt_a("x", 2)"#]);
    assert_eq!(rows("sum_n", &facts), vec![r#"sum_n("x", 2)"#]);
    Ok(())
}
