//! Integration tests for the M8 surface: schema cache + precise
//! unknown-relation errors + `mangle_schema_snapshot` / `mangle_relation_names`.

use mangle_ffi::{
    MANGLE_COMPRESSION_NONE, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES,
    MANGLE_ERR_UNKNOWN_RELATION, MANGLE_OK, MangleBuffer, MangleCursor, MangleEngine,
    MangleValBuilder, mangle_buffer_free, mangle_engine_free, mangle_engine_new,
    mangle_insert_fact, mangle_last_error, mangle_load_facts_mgr, mangle_load_rules, mangle_query,
    mangle_query_dump_mgr, mangle_relation_names, mangle_retract_fact, mangle_save_relation_mgr,
    mangle_schema_snapshot, mangle_val_build_i64, mangle_val_builder_free, mangle_val_builder_new,
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

fn snapshot(engine: *mut MangleEngine) -> serde_json::Value {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_schema_snapshot(engine, &mut buf) };
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let v: serde_json::Value = serde_json::from_slice(slice).unwrap();
    unsafe { mangle_buffer_free(&mut buf) };
    v
}

fn relation_names(engine: *mut MangleEngine) -> Vec<String> {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_relation_names(engine, &mut buf) };
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let v: Vec<String> = serde_json::from_slice(slice).unwrap();
    unsafe { mangle_buffer_free(&mut buf) };
    v
}

const REACHABLE: &str = "\
edge(1, 2).
edge(2, 3).
reachable(X, Y) :- edge(X, Y).
reachable(X, Z) :- edge(X, Y), reachable(Y, Z).
";

// ---- mangle_schema_snapshot ------------------------------------------

#[test]
fn schema_snapshot_for_recursive_program() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, REACHABLE);

    let v = snapshot(p);
    let preds = v["predicates"].as_array().expect("predicates is an array");
    assert_eq!(preds.len(), 2, "edge + reachable");

    // Predicates are sorted by name. Find by name to be order-stable.
    let edge = preds
        .iter()
        .find(|p| p["name"] == "edge")
        .expect("edge present");
    assert_eq!(edge["arity"], 2);
    assert_eq!(edge["kind"], "edb");
    assert!(edge["type_args"].is_null(), "type_args is null in M8");

    let reachable = preds
        .iter()
        .find(|p| p["name"] == "reachable")
        .expect("reachable present");
    assert_eq!(reachable["arity"], 2);
    assert_eq!(reachable["kind"], "idb");

    let rules = v["rules"].as_array().expect("rules is an array");
    assert_eq!(rules.len(), 2, "two rules for reachable");
    // Each rule has rule_id, head, body.
    assert_eq!(rules[0]["rule_id"], 0);
    assert_eq!(rules[0]["head"], "reachable");
    assert!(rules[0]["body"].is_array());
    assert_eq!(rules[1]["rule_id"], 1);
    assert_eq!(rules[1]["head"], "reachable");

    // The recursive rule mentions both `edge` and `reachable` in its
    // body; the base case mentions only `edge`.
    let bodies: Vec<&serde_json::Value> = rules.iter().map(|r| &r["body"]).collect();
    let has_self_edge = bodies.iter().any(|b| {
        let arr = b.as_array().unwrap();
        arr.iter().any(|n| n == "reachable")
    });
    assert!(
        has_self_edge,
        "recursive rule references reachable: {bodies:?}"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn schema_snapshot_edb_only_program() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "node(1). node(2). node(3).");
    let v = snapshot(p);
    let preds = v["predicates"].as_array().unwrap();
    assert_eq!(preds.len(), 1);
    assert_eq!(preds[0]["name"], "node");
    assert_eq!(preds[0]["arity"], 1);
    assert_eq!(preds[0]["kind"], "edb");
    let rules = v["rules"].as_array().unwrap();
    assert!(rules.is_empty(), "EDB-only program has no rules");
    unsafe { mangle_engine_free(p) };
}

#[test]
fn schema_snapshot_no_rules_loaded_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_schema_snapshot(p, &mut buf) };
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn schema_snapshot_null_engine_returns_invalid_arg() {
    drain_last_error();
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_schema_snapshot(ptr::null_mut(), &mut buf) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

// ---- mangle_relation_names ------------------------------------------

#[test]
fn relation_names_includes_declared_but_empty_predicates() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // `unused` predicate appears only as a rule body reference — no
    // facts, no head — but the schema should still know it.
    load(p, "edge(1, 2). reachable(X, Y) :- edge(X, Y), unused(X).");
    let names = relation_names(p);
    assert!(names.contains(&"edge".to_string()), "got: {names:?}");
    assert!(names.contains(&"reachable".to_string()), "got: {names:?}");
    assert!(
        names.contains(&"unused".to_string()),
        "declared-but-empty predicate appears: {names:?}"
    );
    unsafe { mangle_engine_free(p) };
}

#[test]
fn relation_names_sorted() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "z(1). a(1). m(1).");
    let names = relation_names(p);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "names should be sorted");
    unsafe { mangle_engine_free(p) };
}

// ---- Unknown-relation errors on every relation-aware entry point ----

#[test]
fn query_unknown_relation_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");

    let q = "nope";
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(p, q.as_ptr(), q.len(), &mut c) };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    let err = read_last_error();
    assert!(
        err.contains("nope"),
        "error should name the bad predicate: {err}"
    );
    assert!(c.is_null(), "no cursor allocated");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_fact_unknown_relation_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");

    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    let v1 = unsafe { mangle_val_build_i64(b, 1) };
    let v2 = unsafe { mangle_val_build_i64(b, 2) };
    let tuple = [v1, v2];
    let r = "nopde".as_bytes();
    let rc = unsafe {
        mangle_insert_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    drain_last_error();

    unsafe { mangle_val_builder_free(b) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn retract_fact_unknown_relation_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");

    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    let v1 = unsafe { mangle_val_build_i64(b, 1) };
    let v2 = unsafe { mangle_val_build_i64(b, 2) };
    let tuple = [v1, v2];
    let r = "nope".as_bytes();
    let rc = unsafe {
        mangle_retract_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    drain_last_error();

    unsafe { mangle_val_builder_free(b) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn save_relation_mgr_unknown_relation_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");
    let mut buf = MangleBuffer::empty();
    let r = "nope".as_bytes();
    let rc = unsafe {
        mangle_save_relation_mgr(p, r.as_ptr(), r.len(), MANGLE_COMPRESSION_NONE, &mut buf)
    };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    drain_last_error();
    unsafe { mangle_buffer_free(&mut buf) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn query_dump_mgr_unknown_relation_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");
    let mut buf = MangleBuffer::empty();
    let q = "nope";
    let out_rel = "result";
    let rc = unsafe {
        mangle_query_dump_mgr(
            p,
            q.as_ptr(),
            q.len(),
            out_rel.as_ptr(),
            out_rel.len(),
            MANGLE_COMPRESSION_NONE,
            &mut buf,
        )
    };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    drain_last_error();
    unsafe { mangle_buffer_free(&mut buf) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn load_facts_mgr_unknown_relation_in_blob_returns_unknown_relation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Engine declares only `edge`. The blob below contains a
    // `nope` predicate that isn't declared — should be rejected.
    load(p, "edge(0, 0).");
    let blob = b"1\nnope 1 1\nnope(42).\n";
    let name = "foo.mgr";
    let mut n: usize = 0;
    let rc = unsafe {
        mangle_load_facts_mgr(
            p,
            blob.as_ptr(),
            blob.len(),
            name.as_ptr(),
            name.len(),
            &mut n,
        )
    };
    assert_eq!(rc, MANGLE_ERR_UNKNOWN_RELATION);
    let err = read_last_error();
    assert!(err.contains("nope"), "got: {err}");
    // Atomic: no partial inserts (n_inserted_out unchanged).
    assert_eq!(n, 0);
    unsafe { mangle_engine_free(p) };
}

// ---- Query empty result writes the schema arity, not zero ------------

#[test]
fn query_dump_mgr_empty_match_uses_schema_arity() {
    // M7 limitation: write_simple_row inferred arity from first
    // tuple → empty result wrote `name 0 0`. M8 uses schema arity.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2). edge(2, 3).");

    let mut buf = MangleBuffer::empty();
    let q = "edge(99, X)"; // matches nothing
    let out_rel = "empty_dump";
    let rc = unsafe {
        mangle_query_dump_mgr(
            p,
            q.as_ptr(),
            q.len(),
            out_rel.as_ptr(),
            out_rel.len(),
            MANGLE_COMPRESSION_NONE,
            &mut buf,
        )
    };
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let text =
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(buf.data, buf.len) }).unwrap();
    assert!(
        text.contains("empty_dump 2 0"),
        "schema arity (2) used despite empty result: {text:?}"
    );
    unsafe { mangle_buffer_free(&mut buf) };
    unsafe { mangle_engine_free(p) };
}

// ---- Reload rebuilds the schema -------------------------------------

#[test]
fn reload_rebuilds_schema() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");
    let before = relation_names(p);
    assert_eq!(before, vec!["edge".to_string()]);

    // Reload with a different schema. `edge` should go away, `node`
    // appears.
    load(p, "node(1).");
    let after = relation_names(p);
    assert_eq!(after, vec!["node".to_string()]);

    unsafe { mangle_engine_free(p) };
}
