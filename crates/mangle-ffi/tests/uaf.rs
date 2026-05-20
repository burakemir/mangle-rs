//! Deliberate use-after-free tests — `#[ignore]`-gated.
//!
//! These intentionally exercise the documented-UB paths so a
//! sanitizer build has something concrete to catch. They are
//! `#[ignore]`-d so the normal `cargo test` suite never runs them:
//! without ASan they read freed memory, which is UB (may crash, may
//! silently return garbage, may appear to pass — all unacceptable in
//! a normal run).
//!
//! Run them under AddressSanitizer to confirm the sanitizer is
//! actually instrumenting our allocations — each should abort with a
//! `heap-use-after-free` report:
//!
//! ```text
//! RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS="detect_leaks=0" \
//!   cargo +nightly test -p mangle-ffi \
//!   --target x86_64-unknown-linux-gnu \
//!   --test uaf -- --ignored
//! ```
//!
//! The CI ASan job runs `builder_handle_after_free` as a canary and
//! asserts the abort happens (if it *doesn't* abort, ASan isn't
//! instrumenting us and the whole job is worthless).
//!
//! Each of the three corresponds to a lifetime invariant documented
//! in the C header:
//!   1. builder handles do not outlive their builder;
//!   2. a cursor's engine must be alive when `cursor_next` is called;
//!   3. a `cursor_col` pointer is valid only until the next
//!      `cursor_next` / `cursor_free`.

use mangle_ffi::{
    MANGLE_OK, MangleCursor, MangleEngine, MangleVal, MangleValBuilder, mangle_cursor_col,
    mangle_cursor_free, mangle_cursor_next, mangle_engine_free, mangle_engine_new,
    mangle_load_rules, mangle_query, mangle_val_build_i64, mangle_val_builder_free,
    mangle_val_builder_new, mangle_val_kind,
};
use std::hint::black_box;
use std::ptr;

fn load(engine: *mut MangleEngine, src: &str) {
    let bytes = src.as_bytes();
    let ptr_s = bytes.as_ptr();
    let len = bytes.len();
    let rc = unsafe { mangle_load_rules(engine, &ptr_s, &len, 1) };
    assert_eq!(rc, MANGLE_OK);
}

/// Invariant 1: a `MangleVal` handle from a builder must not be used
/// after the builder is freed (the value's `Box` is dropped by
/// `mangle_val_builder_free`).
#[test]
#[ignore = "deliberate use-after-free; run under ASan only"]
fn builder_handle_after_free() {
    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    let v: *const MangleVal = unsafe { mangle_val_build_i64(b, 42) };
    unsafe { mangle_val_builder_free(b) };
    // UAF: `v` points into a Box that builder_free just dropped.
    let kind = unsafe { mangle_val_kind(v) };
    black_box(kind);
}

/// Invariant 2: `mangle_cursor_next` dereferences the engine pointer
/// (to read the generation counter). Calling it after the engine is
/// freed is UB.
#[test]
#[ignore = "deliberate use-after-free; run under ASan only"]
fn cursor_next_after_engine_free() {
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    load(p, "edge(1, 2). edge(2, 3).");

    let q = "edge";
    let mut c: *mut MangleCursor = ptr::null_mut();
    let rc = unsafe { mangle_query(p, q.as_ptr(), q.len(), &mut c) };
    assert_eq!(rc, MANGLE_OK);

    // Free the engine while the cursor is still alive (violates the
    // documented free-cursors-before-engines contract).
    unsafe { mangle_engine_free(p) };

    // UAF: cursor_next reads (*engine).generation from freed memory.
    let rc = unsafe { mangle_cursor_next(c) };
    black_box(rc);
    unsafe { mangle_cursor_free(c) };
}

/// Invariant 3: a `cursor_col` pointer is valid only until the next
/// `cursor_next`. After advancing, the previous row's `Vec<Value>` is
/// dropped, so the old column pointer dangles.
#[test]
#[ignore = "deliberate use-after-free; run under ASan only"]
fn cursor_col_pointer_after_next() {
    let mut p: *mut MangleEngine = ptr::null_mut();
    unsafe { mangle_engine_new(0, &mut p) };
    // Need >= 2 rows so the second cursor_next replaces (and drops)
    // the first row's buffer.
    load(p, "edge(1, 2). edge(2, 3).");

    let q = "edge";
    let mut c: *mut MangleCursor = ptr::null_mut();
    unsafe { mangle_query(p, q.as_ptr(), q.len(), &mut c) };

    assert_eq!(unsafe { mangle_cursor_next(c) }, MANGLE_OK);
    // Grab a pointer into row 0's buffer.
    let col0: *const MangleVal = unsafe { mangle_cursor_col(c, 0) };
    assert!(!col0.is_null());

    // Advance: row 0's Vec<Value> is dropped, freeing its heap buffer.
    assert_eq!(unsafe { mangle_cursor_next(c) }, MANGLE_OK);

    // UAF: col0 points into the freed row-0 buffer.
    let kind = unsafe { mangle_val_kind(col0) };
    black_box(kind);

    unsafe { mangle_cursor_free(c) };
    unsafe { mangle_engine_free(p) };
}
