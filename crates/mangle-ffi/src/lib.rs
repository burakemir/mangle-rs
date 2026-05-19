//! C ABI for the Mangle logic-programming engine.
//!
//! This crate is a stable extern "C" surface over `mangle-driver` and
//! friends, intended for foreign-language consumers (the .NET workbench,
//! csbindgen, other generators). See `include/mangle.h` for the generated
//! header.
//!
//! Milestone status: this is M0 (skeleton). Only [`mangle_version`] and
//! the [`MangleBuffer`] plumbing are implemented. Engine lifecycle, rule
//! loading, queries, etc. land in later milestones — see the companion
//! impl-plan note.

mod buffer;

pub use buffer::{MangleBuffer, mangle_buffer_free};

use buffer::write_buffer;

// ---------------------------------------------------------------------------
// C smoke test linkage.
//
// The C source at `tests/c_smoke/main.c` is built into a static archive by
// `build.rs` via cc::Build. cargo's `rustc-link-lib=static=mangle_c_smoke`
// directive only propagates to dependent binaries (like integration tests)
// when *something* in this crate's rlib references a symbol from the
// archive. Without a reference, the linker drops the directive and the
// integration test fails to find `c_smoke_run`.
//
// The function below provides that reference. It is `pub` so the
// integration test can call it through the crate. The symbol is internal —
// not part of the stable C ABI — and is not declared in `mangle.h`.
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn c_smoke_run() -> i32;
}

/// Run the C smoke test driver from `tests/c_smoke/main.c`.
///
/// Returns 0 on success; nonzero codes indicate which assertion in the C
/// driver failed (see the C source for the code-to-step mapping). This is
/// only used by the integration test in `tests/c_smoke.rs` and is not part
/// of the public ABI.
#[doc(hidden)]
pub fn run_c_smoke() -> i32 {
    unsafe { c_smoke_run() }
}

/// FFI status: success.
pub const MANGLE_OK: i32 = 0;

/// FFI status: an argument was invalid (null pointer, malformed UTF-8,
/// wrong kind for an accessor, etc.).
pub const MANGLE_ERR_INVALID_ARG: i32 = -2;

/// Write the library's semantic version as a UTF-8 string into `out`.
///
/// Always succeeds unless `out` is null. The returned buffer is owned by
/// the caller and must be released with [`mangle_buffer_free`].
///
/// # Safety
/// `out` must be non-null and point to a writable [`MangleBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_version(out: *mut MangleBuffer) -> i32 {
    if out.is_null() {
        return MANGLE_ERR_INVALID_ARG;
    }
    let version = env!("CARGO_PKG_VERSION").as_bytes().to_vec();
    unsafe { write_buffer(out, version) };
    MANGLE_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn version_returns_nonempty_buffer() {
        let mut buf = MangleBuffer::empty();
        let rc = unsafe { mangle_version(&mut buf) };
        assert_eq!(rc, MANGLE_OK);
        assert!(!buf.data.is_null());
        assert!(buf.len > 0);
        assert!(buf.cap >= buf.len);

        // Bytes match the crate version.
        let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
        let s = std::str::from_utf8(slice).expect("version is valid UTF-8");
        assert_eq!(s, env!("CARGO_PKG_VERSION"));

        unsafe { mangle_buffer_free(&mut buf) };
        assert!(buf.data.is_null());
        assert_eq!(buf.len, 0);
        assert_eq!(buf.cap, 0);
    }

    #[test]
    fn version_with_null_out_returns_invalid_arg() {
        let rc = unsafe { mangle_version(ptr::null_mut()) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    }

    #[test]
    fn buffer_free_null_is_noop() {
        unsafe { mangle_buffer_free(ptr::null_mut()) };
    }

    #[test]
    fn buffer_free_empty_is_noop() {
        let mut buf = MangleBuffer::empty();
        unsafe { mangle_buffer_free(&mut buf) };
        // Still empty.
        assert!(buf.data.is_null());
        assert_eq!(buf.len, 0);
        assert_eq!(buf.cap, 0);
    }

    #[test]
    fn buffer_free_idempotent() {
        let mut buf = MangleBuffer::empty();
        let _ = unsafe { mangle_version(&mut buf) };
        unsafe { mangle_buffer_free(&mut buf) };
        // Second free on the now-zeroed struct must not crash.
        unsafe { mangle_buffer_free(&mut buf) };
    }
}
