//! Integration tests for the M4 surface: query + cursor.

use mangle_ffi::{
    MANGLE_ERR_CURSOR_INVALIDATED, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE,
    MANGLE_OK, MANGLE_VAL_NUMBER, MANGLE_VAL_STRING, MangleBuffer, MangleCursor, MangleEngine,
    MangleVal, force_panic_with_engine, mangle_buffer_free, mangle_cursor_arity, mangle_cursor_col,
    mangle_cursor_free, mangle_cursor_next, mangle_engine_free, mangle_engine_new,
    mangle_last_error, mangle_load_rules, mangle_query, mangle_val_as_i64, mangle_val_as_str,
    mangle_val_kind,
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
    assert_eq!(rc, MANGLE_OK, "mangle_query failed: {}", read_last_error());
    assert!(!c.is_null());
    c
}

fn read_str(v: *const MangleVal) -> String {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_val_as_str(v, &mut buf) };
    assert_eq!(rc, MANGLE_OK);
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let s = std::str::from_utf8(slice).unwrap().to_string();
    unsafe { mangle_buffer_free(&mut buf) };
    s
}

fn collect_i64_cols(cursor: *mut MangleCursor, n_cols: usize) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    loop {
        let rc = unsafe { mangle_cursor_next(cursor) };
        if rc == 1 {
            break;
        }
        assert_eq!(rc, MANGLE_OK, "cursor_next: {}", read_last_error());
        let mut row = Vec::with_capacity(n_cols);
        for i in 0..n_cols {
            let v = unsafe { mangle_cursor_col(cursor, i as u32) };
            assert!(!v.is_null());
            let mut n = 0_i64;
            assert_eq!(unsafe { mangle_val_as_i64(v, &mut n) }, MANGLE_OK);
            row.push(n);
        }
        out.push(row);
    }
    out
}

const EDGES: &str = "\
edge(1, 2).
edge(2, 3).
edge(3, 4).
reachable(X, Y) :- edge(X, Y).
reachable(X, Z) :- edge(X, Y), reachable(Y, Z).
";

const ROUTES: &str = r#"
route("GET", "/").
route("GET", "/api").
route("POST", "/login").
route("DELETE", "/users").
"#;

// ---- Happy-path queries ----------------------------------------------

#[test]
fn bare_predicate_returns_all_facts() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let c = open_cursor(p, "edge");
    let mut rows = collect_i64_cols(c, 2);
    rows.sort();
    assert_eq!(rows, vec![vec![1, 2], vec![2, 3], vec![3, 4]]);
    unsafe { mangle_cursor_free(c) };

    let c = open_cursor(p, "reachable");
    let rows = collect_i64_cols(c, 2);
    // reachable is transitive closure: (1,2)(2,3)(3,4)(1,3)(2,4)(1,4)
    assert_eq!(rows.len(), 6);
    unsafe { mangle_cursor_free(c) };

    unsafe { mangle_engine_free(p) };
}

#[test]
fn constant_filter_query() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, ROUTES);

    // route("GET", X) — should yield two rows.
    let c = open_cursor(p, r#"route("GET", X)"#);
    let mut rows: Vec<(String, String)> = Vec::new();
    loop {
        let rc = unsafe { mangle_cursor_next(c) };
        if rc == 1 {
            break;
        }
        assert_eq!(rc, MANGLE_OK);
        assert_eq!(unsafe { mangle_cursor_arity(c) }, 2);
        let m = unsafe { mangle_cursor_col(c, 0) };
        let p_col = unsafe { mangle_cursor_col(c, 1) };
        assert_eq!(unsafe { mangle_val_kind(m) }, MANGLE_VAL_STRING);
        assert_eq!(unsafe { mangle_val_kind(p_col) }, MANGLE_VAL_STRING);
        rows.push((read_str(m), read_str(p_col)));
    }
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("GET".to_string(), "/".to_string()),
            ("GET".to_string(), "/api".to_string()),
        ]
    );
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn variable_wildcard_filters_correct_position() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, ROUTES);

    // route(M, "/") — matches GET / only.
    let c = open_cursor(p, r#"route(M, "/")"#);
    let mut count = 0;
    loop {
        let rc = unsafe { mangle_cursor_next(c) };
        if rc == 1 {
            break;
        }
        assert_eq!(rc, MANGLE_OK);
        let m = unsafe { mangle_cursor_col(c, 0) };
        let p_col = unsafe { mangle_cursor_col(c, 1) };
        assert_eq!(read_str(m), "GET");
        assert_eq!(read_str(p_col), "/");
        count += 1;
    }
    assert_eq!(count, 1);
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn empty_relation_returns_no_rows() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Declare `empty` via a rule but never produce any facts.
    load(p, "edge(1,2). empty(X) :- edge(X, 0).");
    let c = open_cursor(p, "empty");
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, 1, "expected immediate end-of-stream");
    assert_eq!(unsafe { mangle_cursor_arity(c) }, 0);
    assert!(unsafe { mangle_cursor_col(c, 0) }.is_null());
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn end_of_stream_is_sticky() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let c = open_cursor(p, "edge(99, X)");
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, 1);
    // Subsequent calls also return 1.
    for _ in 0..5 {
        assert_eq!(unsafe { mangle_cursor_next(c) }, 1);
    }
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn integer_values_via_cursor_col() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let c = open_cursor(p, "edge(1, X)");
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, MANGLE_OK);
    assert_eq!(unsafe { mangle_cursor_arity(c) }, 2);
    let first = unsafe { mangle_cursor_col(c, 0) };
    assert_eq!(unsafe { mangle_val_kind(first) }, MANGLE_VAL_NUMBER);
    let mut n = 0_i64;
    unsafe { mangle_val_as_i64(first, &mut n) };
    assert_eq!(n, 1);
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

// ---- Concurrency: multiple cursors over the same engine --------------

#[test]
fn many_cursors_coexist() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);

    let mut cursors: Vec<*mut MangleCursor> = (0..4).map(|_| open_cursor(p, "edge")).collect();
    // Advance each cursor independently and verify they all see 3 rows.
    for c in cursors.iter() {
        let rows = collect_i64_cols(*c, 2);
        assert_eq!(rows.len(), 3);
    }
    for c in cursors.drain(..) {
        unsafe { mangle_cursor_free(c) };
    }
    unsafe { mangle_engine_free(p) };
}

// ---- Error paths -----------------------------------------------------

#[test]
fn query_on_engine_without_rules_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let q = "edge";
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(p, q.as_ptr(), q.len(), &mut c) };
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    assert!(c.is_null());
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn malformed_query_returns_parse_error() {
    // The lenient parser falls back to extracting just the predicate
    // name, so even very garbled input usually succeeds. The single
    // case that does fail is fully empty input.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let q = "   ";
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(p, q.as_ptr(), q.len(), &mut c) };
    assert_eq!(rc, MANGLE_ERR_PARSE);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn null_engine_returns_invalid_arg() {
    drain_last_error();
    let q = "edge";
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(ptr::null_mut(), q.as_ptr(), q.len(), &mut c) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn null_out_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let q = "edge";
    let rc = unsafe { mangle_query(p, q.as_ptr(), q.len(), ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn cursor_next_on_null_returns_invalid_arg() {
    drain_last_error();
    let rc = unsafe { mangle_cursor_next(ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn cursor_arity_on_null_returns_minus_one() {
    assert_eq!(unsafe { mangle_cursor_arity(ptr::null_mut()) }, -1);
}

#[test]
fn cursor_col_on_null_returns_null() {
    assert!(unsafe { mangle_cursor_col(ptr::null_mut(), 0) }.is_null());
}

#[test]
fn cursor_free_on_null_is_noop() {
    unsafe { mangle_cursor_free(ptr::null_mut()) };
}

// ---- Generation invalidation -----------------------------------------

#[test]
fn reload_invalidates_open_cursor() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let c = open_cursor(p, "edge");

    // Read one row — works.
    assert_eq!(unsafe { mangle_cursor_next(c) }, MANGLE_OK);

    // Reload rules → generation bumps.
    load(p, EDGES);

    // Next call returns invalidated; cursor_free still works.
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, MANGLE_ERR_CURSOR_INVALIDATED);
    let err = read_last_error();
    assert!(err.contains("invalidated"), "got: {err}");

    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

#[test]
fn panic_invalidates_open_cursor() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    let c = open_cursor(p, "edge");

    // Force a panic on the engine; the panic_boundary macro bumps the
    // generation in addition to setting poisoned.
    unsafe { force_panic_with_engine(p) };
    drain_last_error();

    // cursor_next sees the new generation and refuses.
    let rc = unsafe { mangle_cursor_next(c) };
    assert_eq!(rc, MANGLE_ERR_CURSOR_INVALIDATED);
    drain_last_error();

    // cursor_free is still safe.
    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}

// ---- Stress -----------------------------------------------------------

#[test]
fn open_iterate_free_cycles() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, EDGES);
    for _ in 0..100 {
        let c = open_cursor(p, "edge");
        let rows = collect_i64_cols(c, 2);
        assert_eq!(rows.len(), 3);
        unsafe { mangle_cursor_free(c) };
    }
    unsafe { mangle_engine_free(p) };
}
