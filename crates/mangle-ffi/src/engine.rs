//! MangleEngine handle and lifecycle.
//!
//! [`MangleEngine`] is the top-level handle the C ABI hands out via
//! [`mangle_engine_new`]. In M1 the engine carries only configuration
//! and a poisoned flag — the actual rule/interpreter state is added in
//! M2. The state machine is:
//!
//!   - **Fresh**: just constructed, no rules. All operations that need
//!     rules return `MANGLE_ERR_NO_RULES` (defined for M2; not yet
//!     exercised here).
//!   - **Loaded** (M2): rules compiled, interpreter executed against a
//!     `MemStore`. Queries and snapshots succeed.
//!   - **Poisoned**: a panic was caught inside an entry point that
//!     operated on this engine. All non-free operations return
//!     `MANGLE_ERR_PANIC`. Recovery: free and create a new engine.

use crate::MANGLE_ERR_INVALID_ARG;
use crate::MANGLE_OK;
use crate::error::{panic_boundary, set_error_msg};

/// Opaque engine handle.
///
/// The `poisoned` flag is `pub(crate)` so the `panic_boundary` macro can
/// read and set it directly across module boundaries; consumers see only
/// an opaque pointer.
pub struct MangleEngine {
    // Read in M2 when `load_rules` wires this through to
    // `Interpreter::with_provenance`. Allow-dead until then.
    #[allow(dead_code)]
    pub(crate) enable_provenance: bool,
    pub(crate) poisoned: bool,
}

impl MangleEngine {
    fn new(enable_provenance: bool) -> Self {
        Self {
            enable_provenance,
            poisoned: false,
        }
    }
}

/// Construct a new engine.
///
/// `enable_provenance` is nonzero to record derivation provenance during
/// rule evaluation (M9 surface); zero disables it. The flag is captured
/// here and consulted later when rules are loaded.
///
/// On success, writes the engine pointer to `*out` and returns
/// [`MANGLE_OK`]. The caller owns the handle and must release it with
/// [`mangle_engine_free`].
///
/// # Safety
/// `out` must be non-null and point to writable storage for a
/// `*mut MangleEngine`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_engine_new(
    enable_provenance: i32,
    out: *mut *mut MangleEngine,
) -> i32 {
    panic_boundary!({
        if out.is_null() {
            set_error_msg("mangle_engine_new: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let engine = Box::new(MangleEngine::new(enable_provenance != 0));
        let ptr = Box::into_raw(engine);
        // SAFETY: out is non-null and writable by the caller's contract.
        unsafe { *out = ptr };
        MANGLE_OK
    })
}

/// Release an engine produced by [`mangle_engine_new`].
///
/// Safe to call on a null pointer or a poisoned engine. After return,
/// the pointer must not be used again. The drop is wrapped in
/// `catch_unwind` so an internal panic during drop cannot propagate
/// across the FFI boundary; any panic message is recorded as the
/// thread-local error.
///
/// # Safety
/// If `engine` is non-null, it must point to a handle previously
/// produced by [`mangle_engine_new`] that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_engine_free(engine: *mut MangleEngine) {
    if engine.is_null() {
        return;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: caller's contract guarantees a live, not-yet-freed
        // pointer.
        drop(unsafe { Box::from_raw(engine) });
    }));
    if let Err(payload) = result {
        crate::error::set_error_from_panic(payload);
    }
}

/// Internal test helper: deliberately panic inside the engine-bound
/// panic boundary so tests can exercise the poisoning path.
///
/// Not exported via cbindgen (no `#[unsafe(no_mangle)]`, no
/// `extern "C"`). Integration tests call it as a regular Rust function.
#[doc(hidden)]
pub unsafe fn force_panic_with_engine(engine: *mut MangleEngine) -> i32 {
    panic_boundary!(engine, {
        panic!("deliberate test panic (engine-bound)");
    })
}

/// Internal test helper: deliberately panic inside the engine-less
/// panic boundary.
#[doc(hidden)]
pub fn force_panic_engineless() -> i32 {
    panic_boundary!({
        panic!("deliberate test panic (engine-less)");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::take_error;
    use crate::{MANGLE_ERR_INVALID_ARG, MANGLE_ERR_PANIC};
    use std::ptr;

    #[test]
    fn new_writes_nonnull_pointer_and_free_releases() {
        let mut p: *mut MangleEngine = ptr::null_mut();
        let rc = unsafe { mangle_engine_new(0, &mut p) };
        assert_eq!(rc, MANGLE_OK);
        assert!(!p.is_null());
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn new_records_provenance_flag() {
        let mut p: *mut MangleEngine = ptr::null_mut();
        let _ = unsafe { mangle_engine_new(1, &mut p) };
        let provenance = unsafe { (*p).enable_provenance };
        assert!(provenance);
        unsafe { mangle_engine_free(p) };

        let mut q: *mut MangleEngine = ptr::null_mut();
        let _ = unsafe { mangle_engine_new(0, &mut q) };
        let provenance = unsafe { (*q).enable_provenance };
        assert!(!provenance);
        unsafe { mangle_engine_free(q) };
    }

    #[test]
    fn new_with_null_out_returns_invalid_arg() {
        let rc = unsafe { mangle_engine_new(0, ptr::null_mut()) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    }

    #[test]
    fn free_on_null_is_noop() {
        unsafe { mangle_engine_free(ptr::null_mut()) };
    }

    #[test]
    fn many_engines_coexist() {
        let mut handles = Vec::new();
        for _ in 0..16 {
            let mut p: *mut MangleEngine = ptr::null_mut();
            assert_eq!(unsafe { mangle_engine_new(0, &mut p) }, MANGLE_OK);
            handles.push(p);
        }
        for p in handles {
            unsafe { mangle_engine_free(p) };
        }
    }

    #[test]
    fn engine_bound_panic_marks_poisoned() {
        let _ = take_error();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };
        let rc = unsafe { force_panic_with_engine(p) };
        assert_eq!(rc, MANGLE_ERR_PANIC);
        assert!(unsafe { (*p).poisoned });
        let err = take_error().expect("error set after panic");
        assert!(err.contains("deliberate test panic"), "got: {err}");
        // free still works on poisoned engine
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn engine_bound_panic_short_circuits_subsequent_calls() {
        let _ = take_error();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };
        // First call panics → engine poisoned.
        unsafe { force_panic_with_engine(p) };
        let _ = take_error();
        // Second call should NOT panic again — the pre-check rejects.
        let rc = unsafe { force_panic_with_engine(p) };
        assert_eq!(rc, MANGLE_ERR_PANIC);
        let err = take_error().expect("error set");
        assert!(err.contains("poisoned"), "got: {err}");
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn engine_bound_with_null_returns_invalid_arg() {
        let _ = take_error();
        let rc = unsafe { force_panic_with_engine(ptr::null_mut()) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    }

    #[test]
    fn engineless_force_panic_returns_panic_code() {
        let _ = take_error();
        let rc = force_panic_engineless();
        assert_eq!(rc, MANGLE_ERR_PANIC);
        let _ = take_error();
    }
}
