//! Integration tests for the M9 surface: `mangle_derivation_tree`.

use mangle_ffi::{
    MANGLE_ERR_FACT_NOT_FOUND, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_PROVENANCE,
    MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK, MangleBuffer, MangleEngine,
    mangle_buffer_free, mangle_derivation_tree, mangle_engine_free, mangle_engine_new,
    mangle_last_error, mangle_load_rules,
};
use std::ptr;

fn drain_last_error() {
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    unsafe { mangle_buffer_free(&mut buf) };
}

fn read_last_error() -> String {
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    let s = if buf.len == 0 {
        String::new()
    } else {
        let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
        std::str::from_utf8(slice).unwrap().to_string()
    };
    unsafe { mangle_buffer_free(&mut buf) };
    s
}

fn load(engine: *mut MangleEngine, src: &str) {
    let bytes = src.as_bytes();
    let ptr_s = bytes.as_ptr();
    let len = bytes.len();
    let rc = unsafe { mangle_load_rules(engine, &ptr_s, &len, 1) };
    assert_eq!(rc, MANGLE_OK, "load_rules: {}", read_last_error());
}

fn tree(engine: *mut MangleEngine, fact: &str, max_depth: u32) -> (i32, Option<serde_json::Value>) {
    let mut buf = MangleBuffer::empty();
    let bytes = fact.as_bytes();
    let rc =
        unsafe { mangle_derivation_tree(engine, bytes.as_ptr(), bytes.len(), max_depth, &mut buf) };
    let v = if rc == MANGLE_OK {
        let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
        Some(serde_json::from_slice(slice).unwrap())
    } else {
        None
    };
    unsafe { mangle_buffer_free(&mut buf) };
    (rc, v)
}

const REACHABLE: &str = "\
edge(1, 2).
edge(2, 3).
reachable(X, Y) :- edge(X, Y).
reachable(X, Z) :- edge(X, Y), reachable(Y, Z).
";

// ---- Happy path ------------------------------------------------------

#[test]
fn linear_derivation_one_step() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) }; // provenance ON
    load(p, REACHABLE);

    // reachable(1, 2) is derived directly from edge(1, 2) via rule 1.
    let (rc, v) = tree(p, "reachable(1, 2)", 5);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let v = v.unwrap();
    assert_eq!(v["fact"]["relation"], "reachable");
    assert_eq!(v["fact"]["tuple"], serde_json::json!([1, 2]));
    let derivations = v["derivations"].as_array().expect("derivations array");
    assert!(
        !derivations.is_empty(),
        "should have at least one derivation"
    );

    // Each derivation lists premises; somewhere in there is edge(1,2).
    let mut found_edge_premise = false;
    for d in derivations {
        let premises = d["premises"].as_array().unwrap();
        for p_node in premises {
            if p_node["fact"]["relation"] == "edge"
                && p_node["fact"]["tuple"] == serde_json::json!([1, 2])
            {
                found_edge_premise = true;
            }
        }
    }
    assert!(found_edge_premise, "edge(1,2) appears as a premise");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn transitive_derivation_two_steps() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);

    // reachable(1, 3) requires two hops: edge(1,2) + reachable(2,3).
    let (rc, v) = tree(p, "reachable(1, 3)", 10);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let v = v.unwrap();
    assert_eq!(v["fact"]["tuple"], serde_json::json!([1, 3]));

    // Walk depth: somewhere in the tree, reachable(2, 3) should appear
    // as a premise (transitive step).
    fn has_fact(node: &serde_json::Value, relation: &str, tuple: &serde_json::Value) -> bool {
        if node["fact"]["relation"] == relation && node["fact"]["tuple"] == *tuple {
            return true;
        }
        if let Some(deriv) = node["derivations"].as_array() {
            for d in deriv {
                if let Some(premises) = d["premises"].as_array() {
                    for p in premises {
                        if has_fact(p, relation, tuple) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
    assert!(
        has_fact(&v, "reachable", &serde_json::json!([2, 3])),
        "reachable(2,3) appears in the derivation tree"
    );
    assert!(
        has_fact(&v, "edge", &serde_json::json!([1, 2])),
        "edge(1,2) appears as a leaf premise"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn max_depth_truncates() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);

    // depth 0: root only, derivations: null.
    let (rc, v) = tree(p, "reachable(1, 3)", 0);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let v = v.unwrap();
    assert!(v["derivations"].is_null(), "truncation marker");

    // depth 1: root has derivations, but each premise is truncated.
    let (rc, v) = tree(p, "reachable(1, 3)", 1);
    assert_eq!(rc, MANGLE_OK);
    let v = v.unwrap();
    let derivations = v["derivations"].as_array().unwrap();
    assert!(!derivations.is_empty());
    for d in derivations {
        for p_node in d["premises"].as_array().unwrap() {
            // Premise subtree's derivations should be null (truncated)
            // unless the premise is an EDB leaf (in which case
            // derivations is []).
            let deriv = &p_node["derivations"];
            assert!(deriv.is_null() || deriv.as_array().is_some_and(|a| a.is_empty()));
        }
    }

    unsafe { mangle_engine_free(p) };
}

// ---- EDB leaves and unknown facts ------------------------------------

#[test]
fn edb_leaf_has_premiseless_derivation() {
    // The provenance recorder fires on every `is_new` insert, which
    // includes EDB facts as the program is initialized. An EDB fact
    // therefore shows up in the index with **one derivation whose
    // premises list is empty** — semantically meaningful as "this
    // fact has no antecedents, it came from a Decl/EDB load." This
    // is distinct from an unknown fact (which returns
    // MANGLE_ERR_FACT_NOT_FOUND).
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);

    let (rc, v) = tree(p, "edge(1, 2)", 10);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let v = v.unwrap();
    let derivations = v["derivations"].as_array().expect("derivations array");
    // At least one derivation, and that derivation's premises are
    // empty (the "EDB leaf" shape).
    assert!(!derivations.is_empty(), "EDB fact has a derivation");
    assert!(
        derivations
            .iter()
            .any(|d| d["premises"].as_array().is_some_and(|p| p.is_empty())),
        "EDB derivation has empty premises: {derivations:?}"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn unknown_fact_returns_fact_not_found() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);

    // edge(99, 100) was never asserted — not in EDB, no derivations.
    let (rc, _) = tree(p, "edge(99, 100)", 10);
    assert_eq!(rc, MANGLE_ERR_FACT_NOT_FOUND);
    drain_last_error();

    unsafe { mangle_engine_free(p) };
}

// ---- Provenance-disabled / no-rules paths ---------------------------

#[test]
fn engine_without_provenance_returns_no_provenance() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) }; // provenance OFF
    load(p, REACHABLE);

    let (rc, _) = tree(p, "reachable(1, 2)", 5);
    assert_eq!(rc, MANGLE_ERR_NO_PROVENANCE);
    let err = read_last_error();
    assert!(err.contains("provenance"), "got: {err}");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn no_rules_loaded_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    // No load_rules call.

    let (rc, _) = tree(p, "reachable(1, 2)", 5);
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();

    unsafe { mangle_engine_free(p) };
}

// ---- Argument validation --------------------------------------------

#[test]
fn null_engine_returns_invalid_arg() {
    drain_last_error();
    let fact = "reachable(1, 2)";
    let mut buf = MangleBuffer::empty();
    let rc =
        unsafe { mangle_derivation_tree(ptr::null_mut(), fact.as_ptr(), fact.len(), 5, &mut buf) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn null_out_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);
    let fact = "reachable(1, 2)";
    let rc = unsafe { mangle_derivation_tree(p, fact.as_ptr(), fact.len(), 5, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn variable_in_fact_returns_parse_error() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);

    let (rc, _) = tree(p, "reachable(X, 2)", 5);
    assert_eq!(rc, MANGLE_ERR_PARSE);
    let err = read_last_error();
    assert!(
        err.contains("variable") || err.contains("ground"),
        "got: {err}"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn empty_fact_returns_parse_error() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(p, REACHABLE);
    let (rc, _) = tree(p, "", 5);
    assert_eq!(rc, MANGLE_ERR_PARSE);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

// ---- Recursive graph with cycles ------------------------------------

#[test]
fn recursive_graph_does_not_loop() {
    // Cycle in the underlying graph (1 → 2 → 1) means reachable
    // facts re-derive via cycles. The derivation walker uses a
    // visited-set so it doesn't infinite-loop.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };
    load(
        p,
        "edge(1, 2). edge(2, 1).\n\
         reachable(X, Y) :- edge(X, Y).\n\
         reachable(X, Z) :- edge(X, Y), reachable(Y, Z).\n",
    );

    let (rc, v) = tree(p, "reachable(1, 1)", 100);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    // Walk completes — derivations array is present and non-empty
    // (the visited-set prevents infinite recursion).
    let v = v.unwrap();
    let derivations = v["derivations"].as_array().unwrap();
    assert!(!derivations.is_empty(), "got: {v}");

    unsafe { mangle_engine_free(p) };
}
