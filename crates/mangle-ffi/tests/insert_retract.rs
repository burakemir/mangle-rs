//! Integration tests for the M5 surface: insert and retract.

use mangle_ffi::{
    MANGLE_COMPOUND_LIST, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_OK,
    MANGLE_VAL_COMPOUND, MANGLE_VAL_NUMBER, MangleBuffer, MangleCursor, MangleEngine, MangleVal,
    MangleValBuilder, mangle_buffer_free, mangle_cursor_col, mangle_cursor_free,
    mangle_cursor_next, mangle_engine_free, mangle_engine_new, mangle_insert_fact,
    mangle_last_error, mangle_load_rules, mangle_query, mangle_retract_fact, mangle_val_as_i64,
    mangle_val_build_compound, mangle_val_build_i64, mangle_val_build_string,
    mangle_val_builder_free, mangle_val_builder_new, mangle_val_compound_get,
    mangle_val_compound_len, mangle_val_kind,
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
    assert_eq!(rc, MANGLE_OK, "load_rules failed: {}", read_last_error());
}

fn open_cursor(engine: *mut MangleEngine, query: &str) -> *mut MangleCursor {
    let bytes = query.as_bytes();
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(engine, bytes.as_ptr(), bytes.len(), &mut c) };
    assert_eq!(rc, MANGLE_OK, "mangle_query: {}", read_last_error());
    c
}

fn count_rows(c: *mut MangleCursor) -> usize {
    let mut n = 0;
    loop {
        let rc = unsafe { mangle_cursor_next(c) };
        if rc == 1 {
            break;
        }
        assert_eq!(rc, MANGLE_OK);
        n += 1;
    }
    n
}

fn insert_pair(
    engine: *mut MangleEngine,
    b: *mut MangleValBuilder,
    relation: &str,
    a: i64,
    c: i64,
) -> i32 {
    let av = unsafe { mangle_val_build_i64(b, a) };
    let cv = unsafe { mangle_val_build_i64(b, c) };
    let tuple = [av, cv];
    let r = relation.as_bytes();
    let mut added: i32 = -1;
    let rc = unsafe {
        mangle_insert_fact(
            engine,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            &mut added,
        )
    };
    assert_eq!(rc, MANGLE_OK, "insert failed: {}", read_last_error());
    added
}

fn retract_pair(
    engine: *mut MangleEngine,
    b: *mut MangleValBuilder,
    relation: &str,
    a: i64,
    c: i64,
) -> i32 {
    let av = unsafe { mangle_val_build_i64(b, a) };
    let cv = unsafe { mangle_val_build_i64(b, c) };
    let tuple = [av, cv];
    let r = relation.as_bytes();
    let mut found: i32 = -1;
    let rc = unsafe {
        mangle_retract_fact(
            engine,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            &mut found,
        )
    };
    assert_eq!(rc, MANGLE_OK, "retract failed: {}", read_last_error());
    found
}

const EDGES: &str = "\
edge(1, 2).
edge(2, 3).
reachable(X, Y) :- edge(X, Y).
reachable(X, Z) :- edge(X, Y), reachable(Y, Z).
";

// ---- Insert -----------------------------------------------------------

#[test]
fn insert_new_tuple_visible_to_subsequent_query() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    // Baseline: 2 edge facts.
    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 2);
    unsafe { mangle_cursor_free(c) };

    // Insert a new edge.
    let b = unsafe { mangle_val_builder_new() };
    let added = insert_pair(p, b, "edge", 3, 4);
    assert_eq!(added, 1, "first insert should report added=true");
    unsafe { mangle_val_builder_free(b) };

    // Fresh cursor sees 3 edges.
    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 3);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn duplicate_insert_returns_added_zero() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let b = unsafe { mangle_val_builder_new() };
    let added = insert_pair(p, b, "edge", 1, 2); // already in EDB
    assert_eq!(added, 0, "duplicate insert should report added=false");
    unsafe { mangle_val_builder_free(b) };

    // Still 2 edges.
    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 2);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_does_not_re_derive_idb() {
    // Per the documented contract: inserting into edge does NOT cause
    // reachable rules to fire. The new edge appears in `edge` but not
    // in `reachable`.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    // Baseline reachable count.
    let c = open_cursor(p, "reachable");
    let baseline = count_rows(c);
    unsafe { mangle_cursor_free(c) };
    assert!(baseline >= 2);

    // Insert a new edge that would extend transitive closure.
    let b = unsafe { mangle_val_builder_new() };
    insert_pair(p, b, "edge", 3, 4);
    unsafe { mangle_val_builder_free(b) };

    // edge gained one entry.
    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 3);
    unsafe { mangle_cursor_free(c) };

    // reachable did NOT.
    let c = open_cursor(p, "reachable");
    assert_eq!(count_rows(c), baseline);
    unsafe { mangle_cursor_free(c) };

    // Reloading rebuilds the IDB.
    load(p, EDGES);
    let c = open_cursor(p, "reachable");
    let after_reload = count_rows(c);
    unsafe { mangle_cursor_free(c) };
    assert_eq!(
        after_reload, baseline,
        "reload (without the inserted edge in the source) resets to baseline"
    );

    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_into_other_relation_does_not_leak() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Two predicates with the same arity.
    load(p, "a(X, Y) :- b(X, Y). b(1, 2).");

    let b = unsafe { mangle_val_builder_new() };
    insert_pair(p, b, "b", 5, 6);
    unsafe { mangle_val_builder_free(b) };

    // `b` has the new tuple; `a` does not (IDB not re-derived).
    let c = open_cursor(p, "b");
    assert_eq!(count_rows(c), 2);
    unsafe { mangle_cursor_free(c) };

    let c = open_cursor(p, "a");
    let n = count_rows(c);
    assert_eq!(n, 1, "a unchanged by insert into b");
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_compound_value_roundtrip() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // A predicate whose arity-2 takes a name and a list.
    load(p, "tagged(/x, [1, 2]).");

    let b = unsafe { mangle_val_builder_new() };
    // Build a list [10, 20, 30].
    let e1 = unsafe { mangle_val_build_i64(b, 10) };
    let e2 = unsafe { mangle_val_build_i64(b, 20) };
    let e3 = unsafe { mangle_val_build_i64(b, 30) };
    let elems = [e1, e2, e3];
    let list =
        unsafe { mangle_val_build_compound(b, MANGLE_COMPOUND_LIST, elems.as_ptr(), elems.len()) };
    // Build name /y.
    let name = "/y";
    let name_v = unsafe { mangle_ffi::mangle_val_build_name(b, name.as_ptr(), name.len()) };
    let tuple = [name_v, list];
    let r = "tagged".as_bytes();
    let mut added: i32 = -1;
    let rc = unsafe {
        mangle_insert_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            &mut added,
        )
    };
    assert_eq!(rc, MANGLE_OK, "insert: {}", read_last_error());
    assert_eq!(added, 1);

    // Query: find the row with the /y name.
    let c = open_cursor(p, r#"tagged(/y, L)"#);
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, MANGLE_OK, "expected a row");
    let l = unsafe { mangle_cursor_col(c, 1) };
    assert!(!l.is_null());
    assert_eq!(unsafe { mangle_val_kind(l) }, MANGLE_VAL_COMPOUND);
    let mut len = 0_usize;
    unsafe { mangle_val_compound_len(l, &mut len) };
    assert_eq!(len, 3);
    for (i, want) in [10_i64, 20, 30].iter().enumerate() {
        let elem = unsafe { mangle_val_compound_get(l, i) };
        assert_eq!(unsafe { mangle_val_kind(elem) }, MANGLE_VAL_NUMBER);
        let mut n = 0_i64;
        unsafe { mangle_val_as_i64(elem, &mut n) };
        assert_eq!(n, *want);
    }
    // End-of-stream.
    assert_eq!(unsafe { mangle_cursor_next(c) }, 1);
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_val_builder_free(b) };
    unsafe { mangle_engine_free(p) };
}

// ---- Retract ----------------------------------------------------------

#[test]
fn retract_existing_tuple_succeeds() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let b = unsafe { mangle_val_builder_new() };
    let found = retract_pair(p, b, "edge", 1, 2);
    assert_eq!(found, 1, "retract of existing tuple reports found=true");
    unsafe { mangle_val_builder_free(b) };

    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 1);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn retract_missing_tuple_reports_not_found() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let b = unsafe { mangle_val_builder_new() };
    let found = retract_pair(p, b, "edge", 99, 100);
    assert_eq!(found, 0, "retract of missing tuple reports found=false");
    unsafe { mangle_val_builder_free(b) };

    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 2);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

// ---- Error paths ------------------------------------------------------

#[test]
fn insert_on_engine_without_rules_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    let b = unsafe { mangle_val_builder_new() };
    let av = unsafe { mangle_val_build_i64(b, 1) };
    let cv = unsafe { mangle_val_build_i64(b, 2) };
    let tuple = [av, cv];
    let r = "edge".as_bytes();
    let mut added: i32 = -1;
    let rc = unsafe {
        mangle_insert_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            &mut added,
        )
    };
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();
    unsafe { mangle_val_builder_free(b) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_null_engine_returns_invalid_arg() {
    drain_last_error();
    let r = "edge".as_bytes();
    let v: [*const MangleVal; 0] = [];
    let rc = unsafe {
        mangle_insert_fact(
            ptr::null_mut(),
            r.as_ptr(),
            r.len(),
            v.as_ptr(),
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn insert_null_tuple_with_nonzero_arity_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let r = "edge".as_bytes();
    let rc = unsafe { mangle_insert_fact(p, r.as_ptr(), r.len(), ptr::null(), 2, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    let err = read_last_error();
    assert!(err.contains("null"), "got: {err}");
    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_null_element_in_tuple_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let b = unsafe { mangle_val_builder_new() };
    let v1 = unsafe { mangle_val_build_i64(b, 1) };
    let tuple: [*const MangleVal; 2] = [v1, ptr::null()];
    let r = "edge".as_bytes();
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
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_val_builder_free(b) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_with_null_added_out_succeeds_silently() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let b = unsafe { mangle_val_builder_new() };
    let v1 = unsafe { mangle_val_build_i64(b, 5) };
    let v2 = unsafe { mangle_val_build_i64(b, 6) };
    let tuple = [v1, v2];
    let r = "edge".as_bytes();
    let rc = unsafe {
        mangle_insert_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            ptr::null_mut(), // discard the added flag
        )
    };
    assert_eq!(rc, MANGLE_OK);
    unsafe { mangle_val_builder_free(b) };

    let c = open_cursor(p, "edge");
    assert_eq!(count_rows(c), 3);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn insert_compound_string_value() {
    // Sanity: string-typed insert too, since EDGES is integer-only.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, r#"route("GET", "/")."#);

    let b = unsafe { mangle_val_builder_new() };
    let m = "POST";
    let path = "/x";
    let mv = unsafe { mangle_val_build_string(b, m.as_ptr(), m.len()) };
    let pv = unsafe { mangle_val_build_string(b, path.as_ptr(), path.len()) };
    let tuple = [mv, pv];
    let r = "route".as_bytes();
    let mut added: i32 = -1;
    let rc = unsafe {
        mangle_insert_fact(
            p,
            r.as_ptr(),
            r.len(),
            tuple.as_ptr(),
            tuple.len(),
            &mut added,
        )
    };
    assert_eq!(rc, MANGLE_OK);
    assert_eq!(added, 1);
    unsafe { mangle_val_builder_free(b) };

    let c = open_cursor(p, "route");
    assert_eq!(count_rows(c), 2);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}
