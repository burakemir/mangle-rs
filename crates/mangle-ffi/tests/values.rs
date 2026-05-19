//! Integration tests for the M3 surface: value handles + builder.

use mangle_ffi::{
    MANGLE_COMPOUND_LIST, MANGLE_COMPOUND_STRUCT, MANGLE_OK, MANGLE_VAL_COMPOUND,
    MANGLE_VAL_DURATION, MANGLE_VAL_FLOAT, MANGLE_VAL_NAME, MANGLE_VAL_NUMBER, MANGLE_VAL_STRING,
    MANGLE_VAL_TIME, MangleBuffer, MangleVal, MangleValBuilder, mangle_buffer_free,
    mangle_last_error, mangle_val_as_f64, mangle_val_as_i64, mangle_val_as_str,
    mangle_val_build_compound, mangle_val_build_duration_ns, mangle_val_build_f64,
    mangle_val_build_i64, mangle_val_build_name, mangle_val_build_null, mangle_val_build_string,
    mangle_val_build_time_ns, mangle_val_builder_free, mangle_val_builder_new,
    mangle_val_compound_get, mangle_val_compound_kind, mangle_val_compound_kv,
    mangle_val_compound_len, mangle_val_kind,
};
use std::ptr;

fn drain_last_error() {
    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    unsafe { mangle_buffer_free(&mut buf) };
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

#[test]
fn all_scalar_kinds_roundtrip() {
    drain_last_error();
    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };
    assert!(!b.is_null());

    // Null.
    let n = unsafe { mangle_val_build_null(b) };
    assert_eq!(unsafe { mangle_val_kind(n) }, 0);

    // Number.
    let n = unsafe { mangle_val_build_i64(b, 1729) };
    assert_eq!(unsafe { mangle_val_kind(n) }, MANGLE_VAL_NUMBER);
    let mut i = 0_i64;
    assert_eq!(unsafe { mangle_val_as_i64(n, &mut i) }, MANGLE_OK);
    assert_eq!(i, 1729);

    // Float.
    let f = unsafe { mangle_val_build_f64(b, -0.125) };
    assert_eq!(unsafe { mangle_val_kind(f) }, MANGLE_VAL_FLOAT);
    let mut g = 0.0_f64;
    assert_eq!(unsafe { mangle_val_as_f64(f, &mut g) }, MANGLE_OK);
    assert_eq!(g, -0.125);

    // String.
    let s = "Wert";
    let sv = unsafe { mangle_val_build_string(b, s.as_ptr(), s.len()) };
    assert_eq!(unsafe { mangle_val_kind(sv) }, MANGLE_VAL_STRING);
    assert_eq!(read_str(sv), s);

    // Name.
    let nm = "/r/admin";
    let nv = unsafe { mangle_val_build_name(b, nm.as_ptr(), nm.len()) };
    assert_eq!(unsafe { mangle_val_kind(nv) }, MANGLE_VAL_NAME);
    assert_eq!(read_str(nv), nm);

    // Time / Duration.
    let t = unsafe { mangle_val_build_time_ns(b, 42_000_000_000) };
    assert_eq!(unsafe { mangle_val_kind(t) }, MANGLE_VAL_TIME);
    let d = unsafe { mangle_val_build_duration_ns(b, 1_500_000) };
    assert_eq!(unsafe { mangle_val_kind(d) }, MANGLE_VAL_DURATION);
    let mut x = 0_i64;
    unsafe { mangle_val_as_i64(t, &mut x) };
    assert_eq!(x, 42_000_000_000);
    unsafe { mangle_val_as_i64(d, &mut x) };
    assert_eq!(x, 1_500_000);

    unsafe { mangle_val_builder_free(b) };
}

#[test]
fn nested_compound_walk() {
    drain_last_error();
    let b: *mut MangleValBuilder = unsafe { mangle_val_builder_new() };

    // Build { /id: 7, /tags: ["alpha", "beta"] }
    let kid = {
        let s = "/id";
        unsafe { mangle_val_build_name(b, s.as_ptr(), s.len()) }
    };
    let id = unsafe { mangle_val_build_i64(b, 7) };

    let ktags = {
        let s = "/tags";
        unsafe { mangle_val_build_name(b, s.as_ptr(), s.len()) }
    };
    let alpha = {
        let s = "alpha";
        unsafe { mangle_val_build_string(b, s.as_ptr(), s.len()) }
    };
    let beta = {
        let s = "beta";
        unsafe { mangle_val_build_string(b, s.as_ptr(), s.len()) }
    };
    let tag_elems = [alpha, beta];
    let tags = unsafe {
        mangle_val_build_compound(b, MANGLE_COMPOUND_LIST, tag_elems.as_ptr(), tag_elems.len())
    };

    let struct_elems = [kid, id, ktags, tags];
    let s = unsafe {
        mangle_val_build_compound(
            b,
            MANGLE_COMPOUND_STRUCT,
            struct_elems.as_ptr(),
            struct_elems.len(),
        )
    };
    assert_eq!(unsafe { mangle_val_kind(s) }, MANGLE_VAL_COMPOUND);

    let mut len = 0_usize;
    unsafe { mangle_val_compound_len(s, &mut len) };
    assert_eq!(len, 2, "struct len is pair count");

    // /id, 7
    let mut k = ptr::null::<MangleVal>();
    let mut v = ptr::null::<MangleVal>();
    unsafe { mangle_val_compound_kv(s, 0, &mut k, &mut v) };
    assert_eq!(read_str(k), "/id");
    let mut n = 0_i64;
    unsafe { mangle_val_as_i64(v, &mut n) };
    assert_eq!(n, 7);

    // /tags, ["alpha", "beta"]
    unsafe { mangle_val_compound_kv(s, 1, &mut k, &mut v) };
    assert_eq!(read_str(k), "/tags");
    let mut sub_kind = -1_i32;
    unsafe { mangle_val_compound_kind(v, &mut sub_kind) };
    assert_eq!(sub_kind, MANGLE_COMPOUND_LIST);
    let mut sub_len = 0_usize;
    unsafe { mangle_val_compound_len(v, &mut sub_len) };
    assert_eq!(sub_len, 2);
    let first = unsafe { mangle_val_compound_get(v, 0) };
    let second = unsafe { mangle_val_compound_get(v, 1) };
    assert_eq!(read_str(first), "alpha");
    assert_eq!(read_str(second), "beta");

    unsafe { mangle_val_builder_free(b) };
}

#[test]
fn name_without_slash_returns_null_and_sets_error() {
    drain_last_error();
    let b = unsafe { mangle_val_builder_new() };
    let s = "admin";
    let v = unsafe { mangle_val_build_name(b, s.as_ptr(), s.len()) };
    assert!(v.is_null());

    let mut buf = MangleBuffer::empty();
    unsafe { mangle_last_error(&mut buf) };
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let msg = std::str::from_utf8(slice).unwrap();
    assert!(msg.contains("'/'"), "got: {msg}");
    unsafe { mangle_buffer_free(&mut buf) };
    unsafe { mangle_val_builder_free(b) };
}

#[test]
fn many_builders_coexist() {
    drain_last_error();
    let mut bs: Vec<*mut MangleValBuilder> = (0..8)
        .map(|_| unsafe { mangle_val_builder_new() })
        .collect();
    // Each builder vends one value; the handles are all distinct.
    let mut handles: Vec<*const MangleVal> = Vec::new();
    for (i, b) in bs.iter().enumerate() {
        let v = unsafe { mangle_val_build_i64(*b, i as i64) };
        assert!(!v.is_null());
        handles.push(v);
    }
    for (i, h) in handles.iter().enumerate() {
        let mut n = 0_i64;
        unsafe { mangle_val_as_i64(*h, &mut n) };
        assert_eq!(n, i as i64);
    }
    for b in bs.drain(..) {
        unsafe { mangle_val_builder_free(b) };
    }
}

#[test]
fn builder_free_null_is_noop() {
    unsafe { mangle_val_builder_free(ptr::null_mut()) };
}
