//! Integration tests for the M7 surface:
//! `mangle_save_facts_mgr`, `mangle_save_relation_mgr`,
//! `mangle_query_dump_mgr`.

use mangle_ffi::{
    MANGLE_COMPRESSION_GZIP, MANGLE_COMPRESSION_NONE, MANGLE_COMPRESSION_ZSTD,
    MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK, MangleBuffer,
    MangleCursor, MangleEngine, mangle_buffer_free, mangle_cursor_next, mangle_engine_free,
    mangle_engine_new, mangle_last_error, mangle_load_facts_mgr, mangle_load_rules, mangle_query,
    mangle_query_dump_mgr, mangle_save_facts_mgr, mangle_save_relation_mgr,
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

fn buf_bytes(buf: &MangleBuffer) -> Vec<u8> {
    if buf.len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(buf.data, buf.len) }.to_vec()
}

fn save_all(engine: *mut MangleEngine, compression: i32) -> (i32, Vec<u8>) {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_save_facts_mgr(engine, compression, &mut buf) };
    let bytes = buf_bytes(&buf);
    unsafe { mangle_buffer_free(&mut buf) };
    (rc, bytes)
}

fn save_relation(engine: *mut MangleEngine, relation: &str, compression: i32) -> (i32, Vec<u8>) {
    let mut buf = MangleBuffer::empty();
    let r = relation.as_bytes();
    let rc =
        unsafe { mangle_save_relation_mgr(engine, r.as_ptr(), r.len(), compression, &mut buf) };
    let bytes = buf_bytes(&buf);
    unsafe { mangle_buffer_free(&mut buf) };
    (rc, bytes)
}

fn query_dump(
    engine: *mut MangleEngine,
    query: &str,
    out_relation: &str,
    compression: i32,
) -> (i32, Vec<u8>) {
    let mut buf = MangleBuffer::empty();
    let q = query.as_bytes();
    let r = out_relation.as_bytes();
    let rc = unsafe {
        mangle_query_dump_mgr(
            engine,
            q.as_ptr(),
            q.len(),
            r.as_ptr(),
            r.len(),
            compression,
            &mut buf,
        )
    };
    let bytes = buf_bytes(&buf);
    unsafe { mangle_buffer_free(&mut buf) };
    (rc, bytes)
}

const EDGES_AND_ROUTES: &str = r#"
edge(1, 2).
edge(2, 3).
edge(3, 4).
route("GET", "/").
route("GET", "/api").
route("POST", "/login").
"#;

// ---- Save + reload round-trip ----------------------------------------

#[test]
fn save_facts_mgr_then_reload_uncompressed() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, EDGES_AND_ROUTES);

    let (rc, bytes) = save_all(p, MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    assert!(!bytes.is_empty(), "saved blob must be non-empty");

    // Reload into a fresh engine with the same predicates declared
    // (need declarations because Store::scan requires the relation to
    // be known to the program).
    let mut q: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut q) };
    load_rules(q, "edge(0, 0). route(\"X\", \"X\").");
    let name = "saved.mgr";
    let mut n_inserted: usize = 0;
    let rc_load = unsafe {
        mangle_load_facts_mgr(
            q,
            bytes.as_ptr(),
            bytes.len(),
            name.as_ptr(),
            name.len(),
            &mut n_inserted,
        )
    };
    assert_eq!(rc_load, MANGLE_OK, "load: {}", read_last_error());
    // 3 edges + 3 routes = 6 tuples loaded (plus the 1+1 declared baseline).
    assert_eq!(n_inserted, 6);

    let edges_after = count_rows(open_cursor(q, "edge"));
    let routes_after = count_rows(open_cursor(q, "route"));
    // Baseline (1+1) + saved (3+3), no overlap with baseline values.
    assert_eq!(edges_after, 4);
    assert_eq!(routes_after, 4);

    unsafe {
        mangle_engine_free(p);
        mangle_engine_free(q);
    }
}

#[test]
fn save_facts_mgr_gzip_smaller_for_redundant_input() {
    // Generate a relation with very redundant data so gzip clearly
    // shrinks it. (gzip overhead means tiny inputs can be *larger*
    // when compressed.)
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // 50 essentially-identical-shape tuples.
    let mut src = String::new();
    src.push_str("edge(0, 0).\n");
    for i in 0..50 {
        src.push_str(&format!("edge({i}, {i}).\n"));
    }
    load_rules(p, &src);

    let (rc_none, plain) = save_all(p, MANGLE_COMPRESSION_NONE);
    assert_eq!(rc_none, MANGLE_OK);
    let (rc_gz, gz) = save_all(p, MANGLE_COMPRESSION_GZIP);
    assert_eq!(rc_gz, MANGLE_OK);

    assert!(
        plain.len() > gz.len(),
        "gzip should shrink redundant input: plain={} gz={}",
        plain.len(),
        gz.len()
    );
    // gzip magic.
    assert_eq!(&gz[..2], &[0x1f, 0x8b]);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn save_facts_mgr_gzip_roundtrips_through_load() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, EDGES_AND_ROUTES);

    let (rc, gz) = save_all(p, MANGLE_COMPRESSION_GZIP);
    assert_eq!(rc, MANGLE_OK);
    assert_eq!(&gz[..2], &[0x1f, 0x8b]);

    // Reload (the read side auto-decompresses via magic-byte sniff).
    let mut q: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut q) };
    load_rules(q, "edge(0, 0). route(\"X\", \"X\").");
    let name = "saved.mgr.gz";
    let mut n: usize = 0;
    let rc_load = unsafe {
        mangle_load_facts_mgr(q, gz.as_ptr(), gz.len(), name.as_ptr(), name.len(), &mut n)
    };
    assert_eq!(rc_load, MANGLE_OK, "{}", read_last_error());
    assert_eq!(n, 6);

    unsafe {
        mangle_engine_free(p);
        mangle_engine_free(q);
    }
}

// ---- Per-relation save -----------------------------------------------

#[test]
fn save_relation_mgr_roundtrip() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, EDGES_AND_ROUTES);

    let (rc, bytes) = save_relation(p, "edge", MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_OK);
    assert!(!bytes.is_empty());

    // Reload into a fresh engine. Verify edge gained the 3 tuples but
    // route is unchanged.
    let mut q: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut q) };
    load_rules(q, "edge(0, 0). route(\"X\", \"X\").");
    let name = "edges.mgr";
    let mut n: usize = 0;
    let rc_load = unsafe {
        mangle_load_facts_mgr(
            q,
            bytes.as_ptr(),
            bytes.len(),
            name.as_ptr(),
            name.len(),
            &mut n,
        )
    };
    assert_eq!(rc_load, MANGLE_OK);
    assert_eq!(n, 3);

    assert_eq!(count_rows(open_cursor(q, "edge")), 4);
    assert_eq!(count_rows(open_cursor(q, "route")), 1);

    unsafe {
        mangle_engine_free(p);
        mangle_engine_free(q);
    }
}

#[test]
fn save_relation_mgr_unknown_relation_emits_empty_blob() {
    // MemStore::scan returns an empty iterator (not Err) for an
    // unknown relation. We mirror that: the saved blob is a valid
    // SimpleRow with one predicate of zero tuples (and arity 0,
    // since arity is inferred from the first tuple).
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");

    let (rc, bytes) = save_relation(p, "no_such_predicate", MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.starts_with("1\n"), "header: {text:?}");
    assert!(
        text.contains("no_such_predicate 0 0"),
        "predicate line: {text:?}"
    );

    unsafe { mangle_engine_free(p) };
}

// ---- Query dump ------------------------------------------------------

#[test]
fn query_dump_mgr_round_trips_filtered_results() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, EDGES_AND_ROUTES);

    // Dump route("GET", X) under the new name `get_routes`.
    let (rc, bytes) = query_dump(
        p,
        r#"route("GET", X)"#,
        "get_routes",
        MANGLE_COMPRESSION_NONE,
    );
    assert_eq!(rc, MANGLE_OK, "{}", read_last_error());
    assert!(!bytes.is_empty());

    // Reload into a fresh engine with the renamed relation declared.
    let mut q: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut q) };
    load_rules(q, "get_routes(\"_\", \"_\").");
    let name = "qdump.mgr";
    let mut n: usize = 0;
    let rc_load = unsafe {
        mangle_load_facts_mgr(
            q,
            bytes.as_ptr(),
            bytes.len(),
            name.as_ptr(),
            name.len(),
            &mut n,
        )
    };
    assert_eq!(rc_load, MANGLE_OK, "{}", read_last_error());
    assert_eq!(n, 2, "two GET routes match");

    // Verify by querying the new relation.
    assert_eq!(count_rows(open_cursor(q, "get_routes")), 3); // 2 loaded + 1 baseline.

    unsafe {
        mangle_engine_free(p);
        mangle_engine_free(q);
    }
}

#[test]
fn query_dump_mgr_empty_result_is_valid() {
    // A query that matches nothing dumps a relation with zero facts.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, EDGES_AND_ROUTES);

    let (rc, bytes) = query_dump(
        p,
        r#"route("PATCH", X)"#,
        "patch_routes",
        MANGLE_COMPRESSION_NONE,
    );
    assert_eq!(rc, MANGLE_OK);
    assert!(!bytes.is_empty(), "even zero-row dumps have a header");

    // The header should declare 1 predicate with 0 facts. Arity is
    // inferred from the first tuple, so a zero-row dump emits arity 0
    // — a known limitation of the SimpleRow format. The output still
    // round-trips to "an empty relation named patch_routes" which is
    // what the consumer asked for.
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.starts_with("1\n"), "header: {text:?}");
    assert!(
        text.contains("patch_routes 0 0"),
        "predicate line: {text:?}"
    );

    unsafe { mangle_engine_free(p) };
}

// ---- Compression behavior --------------------------------------------

#[test]
fn zstd_compression_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");

    let (rc, _) = save_all(p, MANGLE_COMPRESSION_ZSTD);
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    let err = read_last_error();
    assert!(err.contains("zstd"), "got: {err}");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn invalid_compression_mode_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");

    let (rc, _) = save_all(p, 99);
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();

    unsafe { mangle_engine_free(p) };
}

// ---- Error paths -----------------------------------------------------

#[test]
fn save_facts_mgr_no_rules_returns_no_rules() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let (rc, _) = save_all(p, MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_ERR_NO_RULES);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn save_facts_mgr_null_out_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");
    let rc = unsafe { mangle_save_facts_mgr(p, MANGLE_COMPRESSION_NONE, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn save_facts_mgr_null_engine_returns_invalid_arg() {
    drain_last_error();
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_save_facts_mgr(ptr::null_mut(), MANGLE_COMPRESSION_NONE, &mut buf) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn query_dump_mgr_empty_out_relation_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");
    let (rc, _) = query_dump(p, "edge", "", MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    let err = read_last_error();
    assert!(err.contains("non-empty"), "got: {err}");
    unsafe { mangle_engine_free(p) };
}

#[test]
fn query_dump_mgr_parse_error_in_query() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load_rules(p, "edge(1, 2).");
    let (rc, _) = query_dump(p, "   ", "result", MANGLE_COMPRESSION_NONE);
    assert_eq!(rc, MANGLE_ERR_PARSE);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}
