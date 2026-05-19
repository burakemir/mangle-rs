//! Integration tests for the M6 surface: `mangle_load_facts_mgr`.
//!
//! Fixtures are generated in-test via `mangle_db::simplerow::write_simple_row`
//! and `flate2`/`ruzstd` for the compressed variants, so we don't carry
//! binary blobs in the repo. The round-trip guarantee is what we
//! actually want to test anyway: bytes that the write side produces
//! must parse cleanly on the read side.

use mangle_common::Value;
use mangle_ffi::{
    MANGLE_ERR, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK,
    MangleBuffer, MangleCursor, MangleEngine, mangle_buffer_free, mangle_cursor_free,
    mangle_cursor_next, mangle_engine_free, mangle_engine_new, mangle_last_error,
    mangle_load_facts_mgr, mangle_load_rules, mangle_query,
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

fn load_rules(engine: *mut MangleEngine, src: &str) {
    let bytes = src.as_bytes();
    let ptr_s = bytes.as_ptr();
    let len = bytes.len();
    let rc = unsafe { mangle_load_rules(engine, &ptr_s, &len, 1) };
    assert_eq!(rc, MANGLE_OK, "load_rules: {}", read_last_error());
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

/// Hand-build a small SimpleRow blob with two predicates: `edge` (arity 2,
/// 3 facts) and `route` (arity 2, 2 facts).
fn build_fixture() -> Vec<u8> {
    let tables: Vec<(String, Vec<Vec<Value>>)> = vec![
        (
            "edge".to_string(),
            vec![
                vec![Value::Number(1), Value::Number(2)],
                vec![Value::Number(2), Value::Number(3)],
                vec![Value::Number(3), Value::Number(4)],
            ],
        ),
        (
            "route".to_string(),
            vec![
                vec![Value::String("GET".into()), Value::String("/".into())],
                vec![Value::String("POST".into()), Value::String("/login".into())],
            ],
        ),
    ];
    let mut buf = Vec::new();
    mangle_db::simplerow::write_simple_row(&mut buf, &tables).expect("write fixture");
    buf
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

fn call_load(engine: *mut MangleEngine, bytes: &[u8]) -> (i32, usize) {
    let name = "test.mgr";
    let mut n: usize = 0;
    let rc = unsafe {
        mangle_load_facts_mgr(
            engine,
            bytes.as_ptr(),
            bytes.len(),
            name.as_ptr(),
            name.len(),
            &mut n,
        )
    };
    (rc, n)
}

// ---- Happy path ------------------------------------------------------

#[test]
fn load_uncompressed_fixture() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Declare both relations via a small program.
    load_rules(p, "edge(1, 2). route(\"GET\", \"/\").");

    // Baseline: 1 edge + 1 route.
    assert_eq!(count_rows(open_cursor(p, "edge")), 1);
    assert_eq!(count_rows(open_cursor(p, "route")), 1);

    let blob = build_fixture();
    let (rc, n) = call_load(p, &blob);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    assert_eq!(n, 5, "fixture has 3 edges + 2 routes = 5 tuples");

    // edge: 1 original + 3 loaded, one of which (1,2) was a duplicate.
    let c = open_cursor(p, "edge");
    let edges = count_rows(c);
    unsafe { mangle_cursor_free(c) };
    assert_eq!(edges, 3, "duplicates collapse silently");

    let c = open_cursor(p, "route");
    let routes = count_rows(c);
    unsafe { mangle_cursor_free(c) };
    assert_eq!(routes, 2);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn load_gzipped_fixture_roundtrips_identically() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0). route(\"X\", \"X\").");

    let blob = build_fixture();
    let gz = gzip(&blob);
    assert!(gz.len() < blob.len() + 64, "gzip overhead reasonable");
    assert!(gz.len() >= 2 && gz[0] == 0x1f && gz[1] == 0x8b);

    let (rc, n) = call_load(p, &gz);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    assert_eq!(n, 5);

    // Set semantics: gzipped vs uncompressed produce identical results.
    let edges = count_rows(open_cursor(p, "edge"));
    let routes = count_rows(open_cursor(p, "route"));
    assert_eq!(edges, 4, "1 baseline + 3 loaded, no overlap");
    assert_eq!(routes, 3);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn empty_mgr_header_only_loads_zero_tuples() {
    // An mgr with `0` predicates (i.e. just the header line) is valid
    // and should load no tuples.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");

    let empty_blob: Vec<u8> = b"0\n".to_vec();
    let (rc, n) = call_load(p, &empty_blob);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    assert_eq!(n, 0);

    // edge still has its baseline tuple.
    assert_eq!(count_rows(open_cursor(p, "edge")), 1);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn null_inserted_out_is_accepted() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0). route(\"X\", \"X\").");
    let blob = build_fixture();
    let name = "x.mgr";
    let rc = unsafe {
        mangle_load_facts_mgr(
            p,
            blob.as_ptr(),
            blob.len(),
            name.as_ptr(),
            name.len(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_OK);
    unsafe { mangle_engine_free(p) };
}

#[test]
fn empty_source_name_is_accepted() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0). route(\"X\", \"X\").");
    let blob = build_fixture();
    let mut n: usize = 0;
    let rc = unsafe { mangle_load_facts_mgr(p, blob.as_ptr(), blob.len(), ptr::null(), 0, &mut n) };
    assert_eq!(rc, MANGLE_OK);
    assert_eq!(n, 5);
    unsafe { mangle_engine_free(p) };
}

// ---- Error paths ----------------------------------------------------

#[test]
fn corrupted_mgr_returns_parse_error() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0).");

    // Header claims 99 predicates but we only provide gibberish.
    let bad = b"99\nnope nothing here\n";
    let (rc, _) = call_load(p, bad);
    assert_eq!(rc, MANGLE_ERR_PARSE, "actual error: {}", read_last_error());
    drain_last_error();

    // State preserved.
    assert_eq!(count_rows(open_cursor(p, "edge")), 1);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn no_rules_loaded_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let blob = build_fixture();
    let (rc, _) = call_load(p, &blob);
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn null_engine_returns_invalid_arg() {
    drain_last_error();
    let blob = build_fixture();
    let name = "x.mgr";
    let rc = unsafe {
        mangle_load_facts_mgr(
            ptr::null_mut(),
            blob.as_ptr(),
            blob.len(),
            name.as_ptr(),
            name.len(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn null_bytes_with_nonzero_len_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0).");
    let rc = unsafe { mangle_load_facts_mgr(p, ptr::null(), 10, ptr::null(), 0, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn invalid_utf8_source_name_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0). route(\"X\", \"X\").");
    let blob = build_fixture();
    let bad_name: [u8; 4] = [0xff, 0xfe, 0xfd, 0xfc];
    let rc = unsafe {
        mangle_load_facts_mgr(
            p,
            blob.as_ptr(),
            blob.len(),
            bad_name.as_ptr(),
            bad_name.len(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    let err = read_last_error();
    assert!(err.contains("UTF-8"), "got: {err}");
    unsafe { mangle_engine_free(p) };
}

#[test]
fn corrupted_gzip_returns_generic_error() {
    // gzip magic but invalid payload → decompress fails → MANGLE_ERR.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(0, 0).");
    let bad: [u8; 8] = [0x1f, 0x8b, 0, 0, 0, 0, 0, 0];
    let (rc, _) = call_load(p, &bad);
    assert_eq!(rc, MANGLE_ERR, "{}", read_last_error());
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}
