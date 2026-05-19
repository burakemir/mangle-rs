//! C ABI for the Mangle logic-programming engine.
//!
//! This crate is a stable extern "C" surface over `mangle-driver` and
//! friends, intended for foreign-language consumers (the .NET workbench,
//! csbindgen, other generators). See `include/mangle.h` for the
//! generated header.
//!
//! Milestone status: M1 (engine lifecycle + last_error + panic
//! poisoning). Implemented entry points:
//!   - [`mangle_version`], [`mangle_buffer_free`] (M0)
//!   - [`mangle_engine_new`], [`mangle_engine_free`] (M1)
//!   - [`mangle_last_error`] (M1)
//!
//! Rule loading, queries, snapshots, etc. land in later milestones — see
//! the companion impl-plan note.

mod buffer;
mod builder;
mod engine;
mod error;
mod value;

pub use buffer::{MangleBuffer, mangle_buffer_free};
pub use builder::{
    MangleValBuilder, mangle_val_build_compound, mangle_val_build_duration_ns,
    mangle_val_build_f64, mangle_val_build_i64, mangle_val_build_name, mangle_val_build_null,
    mangle_val_build_string, mangle_val_build_time_ns, mangle_val_builder_free,
    mangle_val_builder_new,
};
pub use engine::{MangleEngine, mangle_engine_free, mangle_engine_new, mangle_load_rules};
pub use error::mangle_last_error;
pub use value::{
    MANGLE_COMPOUND_LIST, MANGLE_COMPOUND_MAP, MANGLE_COMPOUND_PAIR, MANGLE_COMPOUND_STRUCT,
    MANGLE_VAL_COMPOUND, MANGLE_VAL_DURATION, MANGLE_VAL_FLOAT, MANGLE_VAL_NAME, MANGLE_VAL_NULL,
    MANGLE_VAL_NUMBER, MANGLE_VAL_STRING, MANGLE_VAL_TIME, MangleVal, mangle_val_as_f64,
    mangle_val_as_i64, mangle_val_as_str, mangle_val_compound_get, mangle_val_compound_kind,
    mangle_val_compound_kv, mangle_val_compound_len, mangle_val_kind,
};

// Re-exported test helpers — not part of the C ABI.
#[doc(hidden)]
pub use engine::{
    engine_generation, engine_has_rules, force_panic_engineless, force_panic_with_engine,
};

use buffer::write_buffer;
use error::{panic_boundary, set_error_msg};

// ---------------------------------------------------------------------------
// Stable error codes. Renumbering any of these is a breaking ABI change;
// adding new codes is not. Codes -3 through -7 are reserved for M2-M9 and
// declared here so the generated header is stable across milestones.
// ---------------------------------------------------------------------------

/// FFI status: success.
pub const MANGLE_OK: i32 = 0;

/// FFI status: generic error. The thread-local error slot (read via
/// `mangle_last_error`) holds the formatted message.
pub const MANGLE_ERR: i32 = -1;

/// FFI status: an argument was invalid (null pointer, malformed UTF-8,
/// wrong kind for an accessor, etc.).
pub const MANGLE_ERR_INVALID_ARG: i32 = -2;

/// FFI status: the engine has no rules loaded. Returned by query and
/// snapshot operations on a Fresh engine. Reserved for M2.
pub const MANGLE_ERR_NO_RULES: i32 = -3;

/// FFI status: the cursor's engine generation no longer matches the
/// engine's current generation (rules reloaded, or a panic poisoned the
/// engine). The cursor handle is still safe to free but produces no
/// more rows. Reserved for M4.
pub const MANGLE_ERR_CURSOR_INVALIDATED: i32 = -4;

/// FFI status: derivation-tree introspection was requested but the
/// engine was constructed with `enable_provenance = 0`. Reserved for M9.
pub const MANGLE_ERR_NO_PROVENANCE: i32 = -5;

/// FFI status: a fact lookup (e.g. for derivation tree) found no
/// matching tuple. Reserved for M9.
pub const MANGLE_ERR_FACT_NOT_FOUND: i32 = -6;

/// FFI status: a parse failure in rule, query, or fact-atom input.
/// Reserved for M2 onwards.
pub const MANGLE_ERR_PARSE: i32 = -7;

/// FFI status: an entry point caught a panic. If the entry point took
/// an engine, that engine is now *poisoned* — subsequent operations on
/// it will short-circuit to this same code. Recovery: free the engine
/// and create a fresh one.
pub const MANGLE_ERR_PANIC: i32 = -8;

// ---------------------------------------------------------------------------
// Entry points carried over from M0, now wrapped in panic_boundary for
// uniformity with the rest of the surface.
// ---------------------------------------------------------------------------

/// Write the library's semantic version as a UTF-8 string into `out`.
///
/// Always succeeds unless `out` is null. The returned buffer is owned
/// by the caller and must be released with [`mangle_buffer_free`].
///
/// # Safety
/// `out` must be non-null and point to a writable [`MangleBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_version(out: *mut MangleBuffer) -> i32 {
    panic_boundary!({
        if out.is_null() {
            set_error_msg("mangle_version: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let version = env!("CARGO_PKG_VERSION").as_bytes().to_vec();
        // SAFETY: out is non-null and points to a writable MangleBuffer
        // by the caller's contract.
        unsafe { write_buffer(out, version) };
        MANGLE_OK
    })
}

// ---------------------------------------------------------------------------
// C smoke test linkage.
//
// See tests/c_smoke/main.c. The C source is compiled into a static
// archive by build.rs; cargo's `rustc-link-lib=static=mangle_c_smoke`
// only propagates to dependent binaries (integration tests) when
// something in this crate's rlib references a symbol from the archive.
// `run_c_smoke` provides that reference. The symbol is internal — not
// part of the stable C ABI — and is not declared in `mangle.h`.
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn c_smoke_run() -> i32;
}

/// Run the C smoke test driver from `tests/c_smoke/main.c`. Returns 0
/// on success; nonzero codes indicate which assertion in the C driver
/// failed (see the C source). Only used by `tests/c_smoke.rs`.
#[doc(hidden)]
pub fn run_c_smoke() -> i32 {
    unsafe { c_smoke_run() }
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

        let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
        assert_eq!(
            std::str::from_utf8(slice).unwrap(),
            env!("CARGO_PKG_VERSION")
        );

        unsafe { mangle_buffer_free(&mut buf) };
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
}
