//! Integration tests for the M2 surface: rule loading + generation
//! counter. Driven through the public C ABI from Rust.

use mangle_ffi::{
    MANGLE_ERR_INVALID_ARG, MANGLE_ERR_PANIC, MANGLE_ERR_PARSE, MANGLE_OK, MangleBuffer,
    MangleEngine, engine_generation, engine_has_rules, force_panic_with_engine, mangle_buffer_free,
    mangle_engine_free, mangle_engine_new, mangle_last_error, mangle_load_rules,
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

fn load_one(engine: *mut MangleEngine, src: &str) -> i32 {
    let bytes = src.as_bytes();
    let ptr = bytes.as_ptr();
    let len = bytes.len();
    unsafe { mangle_load_rules(engine, &ptr, &len, 1) }
}

const REACHABLE: &str = "\
edge(1, 2).
edge(2, 3).
edge(3, 4).
reachable(X, Y) :- edge(X, Y).
reachable(X, Z) :- edge(X, Y), reachable(Y, Z).
";

#[test]
fn happy_path_load() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    assert_eq!(load_one(p, REACHABLE), MANGLE_OK);
    assert!(unsafe { engine_has_rules(p) });
    assert_eq!(unsafe { engine_generation(p) }, 1);

    unsafe { mangle_engine_free(p) };
}

#[test]
fn reload_bumps_generation() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    for expected in 1..=5_u64 {
        assert_eq!(load_one(p, REACHABLE), MANGLE_OK);
        assert_eq!(unsafe { engine_generation(p) }, expected);
    }

    unsafe { mangle_engine_free(p) };
}

#[test]
fn parse_error_preserves_state() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    // Establish a baseline good state.
    assert_eq!(load_one(p, REACHABLE), MANGLE_OK);
    let gen_before = unsafe { engine_generation(p) };
    assert!(gen_before >= 1);

    // Garbage input — driver compile_units returns an error.
    let rc = load_one(p, "@@@ this is not mangle @@@");
    assert_eq!(rc, MANGLE_ERR_PARSE);

    let err = read_last_error();
    assert!(!err.is_empty(), "error message should be populated");

    // State unchanged.
    assert_eq!(unsafe { engine_generation(p) }, gen_before);
    assert!(unsafe { engine_has_rules(p) });

    unsafe { mangle_engine_free(p) };
}

#[test]
fn panic_bumps_generation_and_poisons() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    assert_eq!(load_one(p, REACHABLE), MANGLE_OK);
    let gen_before = unsafe { engine_generation(p) };

    // Force a panic.
    let rc = unsafe { force_panic_with_engine(p) };
    assert_eq!(rc, MANGLE_ERR_PANIC);

    // Both poisoned AND generation bumped (so cursors stamped before
    // the panic see the invalidated state on their next call).
    assert_eq!(unsafe { engine_generation(p) }, gen_before + 1);
    drain_last_error();

    // Subsequent load is refused (poisoned).
    let rc2 = load_one(p, REACHABLE);
    assert_eq!(rc2, MANGLE_ERR_PANIC);
    let err = read_last_error();
    assert!(err.contains("poisoned"), "got: {err}");

    unsafe { mangle_engine_free(p) };
}

#[test]
fn null_engine_returns_invalid_arg() {
    drain_last_error();
    let s = "edge(1,2).";
    let ptr_s = s.as_bytes().as_ptr();
    let len = s.len();
    let rc = unsafe { mangle_load_rules(ptr::null_mut(), &ptr_s, &len, 1) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
}

#[test]
fn zero_sources_returns_invalid_arg() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    let rc = unsafe { mangle_load_rules(p, ptr::null(), ptr::null(), 0) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    drain_last_error();
    unsafe { mangle_engine_free(p) };
}

#[test]
fn provenance_enabled_engine_loads() {
    // M2 should at least successfully compile/execute with the
    // provenance flag set. M9 will exercise the recording path.
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(1, &mut p) };

    assert_eq!(load_one(p, REACHABLE), MANGLE_OK);
    assert_eq!(unsafe { engine_generation(p) }, 1);

    unsafe { mangle_engine_free(p) };
}
