//! Integration tests for the M1 surface: engine new/free, last_error,
//! and panic poisoning. Driven through the public C ABI from Rust.

use mangle_ffi::{
    MANGLE_ERR_INVALID_ARG, MANGLE_ERR_PANIC, MANGLE_OK, MangleBuffer, MangleEngine,
    force_panic_engineless, force_panic_with_engine, mangle_buffer_free, mangle_engine_free,
    mangle_engine_new, mangle_last_error,
};
use std::ptr;

fn drain_last_error() {
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    unsafe { mangle_buffer_free(&mut buf) };
}

#[test]
fn engine_new_and_free() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    let rc = unsafe { mangle_engine_new(0, &mut p) };
    assert_eq!(rc, MANGLE_OK);
    assert!(!p.is_null());
    unsafe { mangle_engine_free(p) };
}

#[test]
fn engine_new_with_null_sets_error() {
    drain_last_error();
    let rc = unsafe { mangle_engine_new(0, ptr::null_mut()) };
    assert_eq!(rc, MANGLE_ERR_INVALID_ARG);

    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let s = std::str::from_utf8(slice).unwrap();
    assert!(s.contains("null"), "expected 'null' in error, got: {s}");
    unsafe { mangle_buffer_free(&mut buf) };
}

#[test]
fn last_error_clears_on_read() {
    drain_last_error();
    let _ = unsafe { mangle_engine_new(0, ptr::null_mut()) };
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    assert!(buf.len > 0);
    unsafe { mangle_buffer_free(&mut buf) };

    let mut buf2 = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf2) };
    assert_eq!(buf2.len, 0, "second read should be empty");
    unsafe { mangle_buffer_free(&mut buf2) };
}

#[test]
fn panic_poisons_engine_and_blocks_further_calls() {
    drain_last_error();
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };

    let rc = unsafe { force_panic_with_engine(p) };
    assert_eq!(rc, MANGLE_ERR_PANIC);

    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let s = std::str::from_utf8(slice).unwrap();
    assert!(s.contains("panic"), "got: {s}");
    unsafe { mangle_buffer_free(&mut buf) };

    // Second call: pre-check rejects without re-running body.
    let rc2 = unsafe { force_panic_with_engine(p) };
    assert_eq!(rc2, MANGLE_ERR_PANIC);
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let s = std::str::from_utf8(slice).unwrap();
    assert!(s.contains("poisoned"), "got: {s}");
    unsafe { mangle_buffer_free(&mut buf) };

    // free still works.
    unsafe { mangle_engine_free(p) };
}

#[test]
fn engineless_panic_returns_panic_code() {
    drain_last_error();
    let rc = force_panic_engineless();
    assert_eq!(rc, MANGLE_ERR_PANIC);
    drain_last_error();
}

#[test]
fn many_engines_coexist() {
    drain_last_error();
    let mut handles: Vec<*mut MangleEngine> = Vec::new();
    for _ in 0..16 {
        let mut p: *mut MangleEngine = ptr::null_mut();
        assert_eq!(unsafe { mangle_engine_new(0, &mut p) }, MANGLE_OK);
        handles.push(p);
    }
    // All pointers distinct.
    for i in 0..handles.len() {
        for j in (i + 1)..handles.len() {
            assert_ne!(handles[i], handles[j]);
        }
    }
    for p in handles {
        unsafe { mangle_engine_free(p) };
    }
}

#[test]
fn alloc_free_cycles() {
    // 1000 cycles to surface any leak the buffer integration test
    // doesn't catch. ASan (when enabled in CI) covers correctness.
    drain_last_error();
    for _ in 0..1000 {
        let mut p: *mut MangleEngine = ptr::null_mut();
        assert_eq!(unsafe { mangle_engine_new(0, &mut p) }, MANGLE_OK);
        unsafe { mangle_engine_free(p) };
    }
}
