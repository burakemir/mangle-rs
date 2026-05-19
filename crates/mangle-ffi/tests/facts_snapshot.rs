//! Integration tests for the M10 surface: `mangle_facts_snapshot`.

use mangle_ffi::{
    MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_OK, MangleBuffer, MangleEngine,
    MangleValBuilder, mangle_buffer_free, mangle_engine_free, mangle_engine_new,
    mangle_facts_snapshot, mangle_insert_fact, mangle_last_error, mangle_load_rules,
    mangle_val_build_compound, mangle_val_build_i64, mangle_val_build_name,
    mangle_val_build_string, mangle_val_builder_free, mangle_val_builder_new,
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

fn snapshot(engine: *mut MangleEngine, limit: u32) -> serde_json::Value {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_facts_snapshot(engine, limit, &mut buf) };
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let v: serde_json::Value = serde_json::from_slice(slice).unwrap();
    unsafe { mangle_buffer_free(&mut buf) };
    v
}

fn relations_by_name(
    v: &serde_json::Value,
) -> std::collections::HashMap<String, &serde_json::Value> {
    let arr = v["relations"].as_array().expect("relations array");
    let mut out = std::collections::HashMap::new();
    for r in arr {
        out.insert(r["name"].as_str().unwrap().to_string(), r);
    }
    out
}

// ---- Basic shape -----------------------------------------------------

#[test]
fn snapshot_includes_every_declared_predicate() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // 3 EDB predicates and 1 IDB.
    load(
        p,
        "edge(1, 2). edge(2, 3). node(1). node(2).\n\
         reachable(X, Y) :- edge(X, Y).\n",
    );

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    assert!(by_name.contains_key("edge"));
    assert!(by_name.contains_key("node"));
    assert!(by_name.contains_key("reachable"));

    assert_eq!(by_name["edge"]["arity"], 2);
    assert_eq!(by_name["edge"]["kind"], "edb");
    assert_eq!(by_name["edge"]["count"], 2);
    assert_eq!(by_name["node"]["arity"], 1);
    assert_eq!(by_name["reachable"]["kind"], "idb");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn snapshot_lists_declared_but_empty_predicates() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // `unused` is referenced in a rule body but never asserted/derived.
    load(
        p,
        "edge(1, 2).\n\
         reachable(X, Y) :- edge(X, Y), unused(X).\n",
    );

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    assert!(
        by_name.contains_key("unused"),
        "declared-but-empty predicate appears: {by_name:?}"
    );
    assert_eq!(by_name["unused"]["count"], 0);
    assert_eq!(
        by_name["unused"]["sample"],
        serde_json::json!([]),
        "empty sample for empty predicate"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn sample_limit_is_honored() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Generate 100 edges.
    let mut src = String::new();
    for i in 0..100 {
        src.push_str(&format!("edge({i}, {}).\n", i + 1));
    }
    load(p, &src);

    let v = snapshot(p, 5);
    let by_name = relations_by_name(&v);
    let edge = by_name["edge"];
    assert_eq!(edge["count"], 100);
    let sample = edge["sample"].as_array().unwrap();
    assert_eq!(sample.len(), 5, "exactly 5 sample entries");
    for entry in sample {
        let tuple = entry["tuple"].as_array().unwrap();
        assert_eq!(tuple.len(), 2);
    }

    unsafe { mangle_engine_free(p) };
}

#[test]
fn limit_zero_means_no_samples() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2). edge(2, 3).");

    let v = snapshot(p, 0);
    let by_name = relations_by_name(&v);
    assert_eq!(by_name["edge"]["count"], 2);
    assert_eq!(by_name["edge"]["sample"], serde_json::json!([]));

    unsafe { mangle_engine_free(p) };
}

// ---- Value encoding --------------------------------------------------

#[test]
fn name_value_encoded_as_tagged_object() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "labeled(/x, 1). labeled(/y, 2).");

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    let sample = by_name["labeled"]["sample"].as_array().unwrap();
    assert!(!sample.is_empty());
    // First column is a Name.
    let first = &sample[0]["tuple"][0];
    assert!(first.is_object(), "Name should be a tagged object: {first}");
    assert!(first.get("name").is_some());
    // Second column is a Number → JSON primitive.
    let second = &sample[0]["tuple"][1];
    assert!(second.is_number());

    unsafe { mangle_engine_free(p) };
}

#[test]
fn string_value_encoded_as_primitive() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, r#"route("GET", "/api")."#);

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    let sample = by_name["route"]["sample"].as_array().unwrap();
    assert_eq!(sample[0]["tuple"][0], "GET");
    assert_eq!(sample[0]["tuple"][1], "/api");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn compound_value_encoded_as_tagged_object() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Declare a relation with a list-typed second arg, then
    // insert a list value via the builder.
    load(p, "tagged(/x, [1, 2]).");

    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    let name = "/y";
    let nv = unsafe { mangle_val_build_name(b, name.as_ptr(), name.len()) };
    let e1 = unsafe { mangle_val_build_i64(b, 10) };
    let e2 = unsafe { mangle_val_build_i64(b, 20) };
    let elems = [e1, e2];
    let list = unsafe {
        mangle_val_build_compound(
            b,
            mangle_ffi::MANGLE_COMPOUND_LIST,
            elems.as_ptr(),
            elems.len(),
        )
    };
    let tuple = [nv, list];
    let r = "tagged".as_bytes();
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
    assert_eq!(rc, MANGLE_OK);
    unsafe { mangle_val_builder_free(b) };

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    let sample = by_name["tagged"]["sample"].as_array().unwrap();
    assert_eq!(sample.len(), 2, "1 baseline + 1 inserted");
    // Each row's second cell should be tagged-compound.
    for row in sample {
        let cell = &row["tuple"][1];
        assert!(cell.is_object(), "compound is a tagged object: {cell}");
        assert_eq!(cell["compound"], "list");
        let elems = cell["elems"].as_array().unwrap();
        assert_eq!(elems.len(), 2);
    }

    unsafe { mangle_engine_free(p) };
}

#[test]
fn string_with_special_chars() {
    // Sanity: strings with spaces and slashes don't get confused with
    // Name's `/`-prefix tagging.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, r#"raw("hello world")."#);

    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    let s = "/looks/like/a/name/but/string";
    let sv = unsafe { mangle_val_build_string(b, s.as_ptr(), s.len()) };
    let tuple = [sv];
    let r = "raw".as_bytes();
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
    assert_eq!(rc, MANGLE_OK);
    unsafe { mangle_val_builder_free(b) };

    let v = snapshot(p, 10);
    let by_name = relations_by_name(&v);
    let sample = by_name["raw"]["sample"].as_array().unwrap();
    // Find the inserted entry — its cell is a primitive string, not an object.
    let found = sample
        .iter()
        .any(|row| row["tuple"][0] == "/looks/like/a/name/but/string");
    assert!(found, "string preserved verbatim: {sample:?}");

    unsafe { mangle_engine_free(p) };
}

// ---- Error paths -----------------------------------------------------

#[test]
fn no_rules_loaded_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_facts_snapshot(p, 10, &mut buf) };
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();
    unsafe { mangle_buffer_free(&mut buf) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn null_engine_returns_invalid_arg() {
    drain_last_error();
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_facts_snapshot(ptr::null_mut(), 10, &mut buf) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn null_out_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2).");
    let rc = unsafe { mangle_facts_snapshot(p, 10, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}
