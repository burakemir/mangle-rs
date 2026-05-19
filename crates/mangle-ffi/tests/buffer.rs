//! Integration tests for `MangleBuffer` lifecycle, driven through the
//! public C ABI from Rust. These complement the in-crate unit tests by
//! exercising the same path a foreign consumer takes.

use mangle_ffi::{MangleBuffer, mangle_buffer_free, mangle_version};

#[test]
fn version_roundtrip() {
    let mut buf = MangleBuffer::empty();
    let rc = unsafe { mangle_version(&mut buf) };
    assert_eq!(rc, 0);
    assert!(!buf.data.is_null());

    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    assert_eq!(
        std::str::from_utf8(slice).unwrap(),
        env!("CARGO_PKG_VERSION")
    );

    unsafe { mangle_buffer_free(&mut buf) };
    assert!(buf.data.is_null());
}

#[test]
fn many_alloc_free_cycles() {
    // 1000 cycles to catch any leak/corruption pattern that would slip
    // past a single-shot test. ASan picks up the slack for memory errors;
    // this just ensures the path is stable under repetition.
    for _ in 0..1000 {
        let mut buf = MangleBuffer::empty();
        let rc = unsafe { mangle_version(&mut buf) };
        assert_eq!(rc, 0);
        unsafe { mangle_buffer_free(&mut buf) };
    }
}
