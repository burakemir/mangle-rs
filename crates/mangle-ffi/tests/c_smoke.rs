//! Drives the C smoke test compiled by `build.rs`.
//!
//! `tests/c_smoke/main.c` is built into a static archive (`libmangle_c_smoke.a`)
//! during the FFI crate's normal build and linked into this test binary.
//! It exposes a single function, `c_smoke_run`, which exercises every
//! M0 entry point through the cbindgen-generated header. If the header
//! drifts from the Rust exports, this file fails to link.

#[test]
fn c_smoke_passes() {
    let rc = mangle_ffi::run_c_smoke();
    assert_eq!(
        rc, 0,
        "C smoke test failed with code {rc}; see tests/c_smoke/main.c for the meaning"
    );
}
