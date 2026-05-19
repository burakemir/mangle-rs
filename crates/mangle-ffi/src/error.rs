//! Thread-local error reporting + panic boundary.
//!
//! Every FFI entry point wraps its body in [`panic_boundary!`]. On a
//! caught panic the macro stores the panic payload's message into the
//! thread-local error slot and returns [`crate::MANGLE_ERR_PANIC`]. The
//! consumer reads the message via [`mangle_last_error`].
//!
//! Engine-bound entry points use the two-argument form of the macro,
//! which additionally marks the engine as poisoned. Subsequent
//! operations on the engine short-circuit to `MANGLE_ERR_PANIC` until
//! it is freed and replaced.

use std::any::Any;
use std::cell::RefCell;

use crate::MANGLE_ERR_INVALID_ARG;
use crate::MANGLE_OK;
use crate::buffer::{MangleBuffer, write_buffer};

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the thread-local error message verbatim. The previous message,
/// if any, is replaced.
pub(crate) fn set_error_msg(msg: impl Into<String>) {
    LAST_ERROR.with(|cell| *cell.borrow_mut() = Some(msg.into()));
}

/// Translate a panic payload (the `Box<dyn Any>` returned by
/// `catch_unwind`) into a human-readable message and store it as the
/// current error.
pub(crate) fn set_error_from_panic(payload: Box<dyn Any + Send>) {
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        format!("panic: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("panic: {s}")
    } else {
        "panic: <non-string payload>".to_string()
    };
    set_error_msg(msg);
}

/// Remove and return the current thread-local error message, if any.
pub(crate) fn take_error() -> Option<String> {
    LAST_ERROR.with(|cell| cell.borrow_mut().take())
}

/// Wrap an entry-point body in `catch_unwind`. On caught panic, set the
/// thread-local error from the payload and return `MANGLE_ERR_PANIC`.
///
/// Two forms:
///   - `panic_boundary!({ body })`: engine-less entry points.
///   - `panic_boundary!(engine_ptr, { body })`: engine-bound entry
///     points. Pre-checks `engine_ptr` for null and for an existing
///     poisoned state; both short-circuit without running the body.
///     On caught panic, marks the engine as poisoned.
macro_rules! panic_boundary {
    // Engine-less variant.
    ($body:block) => {{
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> i32 { $body }));
        match result {
            Ok(rc) => rc,
            Err(payload) => {
                $crate::error::set_error_from_panic(payload);
                $crate::MANGLE_ERR_PANIC
            }
        }
    }};
    // Engine-bound variant.
    ($engine:expr, $body:block) => {{
        let engine_ptr: *mut $crate::engine::MangleEngine = $engine;
        if engine_ptr.is_null() {
            $crate::error::set_error_msg("engine pointer is null");
            return $crate::MANGLE_ERR_INVALID_ARG;
        }
        // SAFETY: the caller's contract is that any non-null engine
        // pointer points to a live Engine produced by mangle_engine_new
        // and not yet freed. Reading the `poisoned` byte is sound under
        // that contract.
        if unsafe { (*engine_ptr).poisoned } {
            $crate::error::set_error_msg(
                "engine poisoned by a previous panic; free and create a new one",
            );
            return $crate::MANGLE_ERR_PANIC;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> i32 { $body }));
        match result {
            Ok(rc) => rc,
            Err(payload) => {
                $crate::error::set_error_from_panic(payload);
                // SAFETY: same contract as above; the engine is still
                // allocated (panic happened inside the closure, not
                // during a free).
                unsafe {
                    (*engine_ptr).poisoned = true;
                }
                $crate::MANGLE_ERR_PANIC
            }
        }
    }};
}

pub(crate) use panic_boundary;

/// Copy the current thread-local error message into `out` and clear it.
///
/// Returns [`MANGLE_OK`] regardless of whether an error was set; the
/// resulting buffer will be empty (zero-length, possibly null `data`)
/// when there was nothing to report. Calling this in a no-error state
/// is the canonical way to check + clear.
///
/// # Safety
/// `out` must be non-null and point to a writable [`MangleBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_last_error(out: *mut MangleBuffer) -> i32 {
    panic_boundary!({
        if out.is_null() {
            set_error_msg("mangle_last_error: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let bytes = take_error().unwrap_or_default().into_bytes();
        // SAFETY: out is non-null and points to a writable MangleBuffer
        // by the caller's contract.
        unsafe { write_buffer(out, bytes) };
        MANGLE_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MANGLE_ERR_PANIC;
    use std::ptr;

    fn clear() {
        LAST_ERROR.with(|cell| *cell.borrow_mut() = None);
    }

    #[test]
    fn last_error_when_unset_returns_empty_buffer() {
        clear();
        let mut buf = MangleBuffer::empty();
        let rc = unsafe { mangle_last_error(&mut buf) };
        assert_eq!(rc, MANGLE_OK);
        assert_eq!(buf.len, 0);
        unsafe { crate::mangle_buffer_free(&mut buf) };
    }

    #[test]
    fn last_error_roundtrips_a_set_message() {
        clear();
        set_error_msg("something went wrong");
        let mut buf = MangleBuffer::empty();
        let rc = unsafe { mangle_last_error(&mut buf) };
        assert_eq!(rc, MANGLE_OK);
        let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
        assert_eq!(std::str::from_utf8(slice).unwrap(), "something went wrong");
        unsafe { crate::mangle_buffer_free(&mut buf) };

        // Second call returns empty: take_error cleared it.
        let mut buf2 = MangleBuffer::empty();
        let rc2 = unsafe { mangle_last_error(&mut buf2) };
        assert_eq!(rc2, MANGLE_OK);
        assert_eq!(buf2.len, 0);
        unsafe { crate::mangle_buffer_free(&mut buf2) };
    }

    #[test]
    fn last_error_with_null_out_returns_invalid_arg() {
        clear();
        let rc = unsafe { mangle_last_error(ptr::null_mut()) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
    }

    #[test]
    fn engineless_panic_boundary_catches_and_reports() {
        clear();
        fn entry() -> i32 {
            panic_boundary!({
                panic!("boom");
            })
        }
        let rc = entry();
        assert_eq!(rc, MANGLE_ERR_PANIC);
        assert_eq!(take_error().as_deref(), Some("panic: boom"));
    }

    #[test]
    fn engineless_panic_boundary_with_string_payload() {
        clear();
        fn entry() -> i32 {
            panic_boundary!({
                panic!("{}", "dynamic payload".to_string());
            })
        }
        let rc = entry();
        assert_eq!(rc, MANGLE_ERR_PANIC);
        let err = take_error().expect("error set");
        assert!(err.contains("dynamic payload"), "got: {err}");
    }

    #[test]
    fn engineless_panic_boundary_passes_through_normal_return() {
        clear();
        fn entry() -> i32 {
            panic_boundary!({ MANGLE_OK })
        }
        assert_eq!(entry(), MANGLE_OK);
        assert!(take_error().is_none());
    }
}
