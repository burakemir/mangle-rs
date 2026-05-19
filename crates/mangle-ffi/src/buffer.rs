//! Owned byte buffers crossing the FFI boundary.
//!
//! [`MangleBuffer`] is the universal output type for entry points that
//! return variable-length data (JSON snapshots, serialized facts, error
//! messages). Caller takes ownership and must release with
//! [`mangle_buffer_free`]. Fields are `#[repr(C)]` so the layout matches
//! the cbindgen-generated struct in `mangle.h`.

use std::ptr;

/// A heap-allocated byte buffer owned by the caller.
///
/// Returned by FFI entry points that produce variable-length output. The
/// caller must release it with [`mangle_buffer_free`]. A zeroed
/// `MangleBuffer` (all fields 0/null) is the canonical "empty / released"
/// state; freeing it is a no-op.
#[repr(C)]
pub struct MangleBuffer {
    pub data: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl MangleBuffer {
    /// The empty/released sentinel value: null data, zero len, zero cap.
    pub const fn empty() -> Self {
        Self {
            data: ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }
}

/// Move ownership of a `Vec<u8>` into a [`MangleBuffer`] by leaking the
/// vec's raw parts. The caller-side [`mangle_buffer_free`] reconstructs
/// the vec to drop it.
pub(crate) fn vec_into_buffer(mut v: Vec<u8>) -> MangleBuffer {
    // Shrink so cap == len; simplifies the cap field's semantics for
    // foreign consumers (they only ever see "useful bytes" worth of data).
    v.shrink_to_fit();
    let mut v = std::mem::ManuallyDrop::new(v);
    MangleBuffer {
        data: v.as_mut_ptr(),
        len: v.len(),
        cap: v.capacity(),
    }
}

/// Write `bytes` into `*out`, transferring ownership to the caller.
///
/// `out` must point to a writable `MangleBuffer` (the caller typically
/// stack-allocates `MangleBuffer buf = {0};`). Any previous contents are
/// overwritten without freeing — callers passing a non-empty buffer leak.
/// This matches the convention that output parameters are "filled in"
/// rather than "appended to".
///
/// # Safety
/// `out` must be non-null and point to a valid `MangleBuffer`.
pub(crate) unsafe fn write_buffer(out: *mut MangleBuffer, bytes: Vec<u8>) {
    if out.is_null() {
        return;
    }
    unsafe {
        ptr::write(out, vec_into_buffer(bytes));
    }
}

/// Release a [`MangleBuffer`] previously produced by the library.
///
/// Safe to call with a null pointer or a zeroed/empty buffer (both
/// no-ops). After return, the buffer's fields are zeroed so a subsequent
/// double-free is also a no-op.
///
/// # Safety
/// If `buf` is non-null, it must point to a buffer that was produced by
/// this library and has not been freed. The buffer's `data`/`len`/`cap`
/// fields must not have been modified by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_buffer_free(buf: *mut MangleBuffer) {
    if buf.is_null() {
        return;
    }
    let b = unsafe { &mut *buf };
    if b.data.is_null() || b.cap == 0 {
        // Already empty / freed: leave zeroed and return.
        b.data = ptr::null_mut();
        b.len = 0;
        b.cap = 0;
        return;
    }
    // Reconstruct + drop.
    unsafe {
        drop(Vec::from_raw_parts(b.data, b.len, b.cap));
    }
    b.data = ptr::null_mut();
    b.len = 0;
    b.cap = 0;
}
