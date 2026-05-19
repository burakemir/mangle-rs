//! Construction-side value handles: [`MangleValBuilder`].
//!
//! The builder is a heap arena that owns the `Value`s the caller is
//! constructing. Each `build_*` function allocates a fresh `Box<Value>`,
//! stores its raw pointer in the builder's arena, and hands the same
//! pointer back as a `*const MangleVal`. The handles stay valid until
//! the builder is freed; on free, every owned allocation is dropped.
//!
//! The Box-per-value approach (rather than a single growable `Vec`)
//! keeps handles stable across subsequent `build_*` calls: even if the
//! arena's bookkeeping `Vec` reallocates, the heap allocations it
//! points to don't move.
//!
//! This is the only producer of handles in M3; M4 will add the cursor
//! row buffer as a second producer with the same `*const MangleVal`
//! contract.

use mangle_common::{CompoundKind, Value};

use crate::error::set_error_msg;
use crate::value::{MangleVal, compound_subkind_from_i32};

/// Arena owning a set of `Value`s constructed via the `build_*` entry
/// points. Hands out `*const MangleVal` handles that are valid until
/// the builder is freed.
pub struct MangleValBuilder {
    arena: Vec<*mut Value>,
}

impl MangleValBuilder {
    fn new() -> Self {
        Self { arena: Vec::new() }
    }

    fn push(&mut self, v: Value) -> *const MangleVal {
        // Box::into_raw transfers ownership to us; the heap allocation
        // is stable, so the returned pointer remains valid across
        // subsequent pushes (even if `arena`'s internal buffer
        // reallocates — that only moves the raw-pointer entries, not
        // the heap allocations they reference).
        let ptr = Box::into_raw(Box::new(v));
        self.arena.push(ptr);
        ptr as *const MangleVal
    }
}

impl Drop for MangleValBuilder {
    fn drop(&mut self) {
        for ptr in self.arena.drain(..) {
            // SAFETY: each ptr came from Box::into_raw on a value we
            // own; reconstituting and dropping the Box frees it.
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

/// Construct a new value builder. Returns null on allocation failure
/// (which is currently unreachable, but the C ABI consumer should still
/// check). Release with [`mangle_val_builder_free`].
///
/// # Safety
/// Safe to call from any context; the `unsafe` marker is for ABI
/// consistency with the other entry points in this crate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_builder_new() -> *mut MangleValBuilder {
    Box::into_raw(Box::new(MangleValBuilder::new()))
}

/// Release a builder. All handles previously returned by `build_*`
/// against this builder become invalid; using them after this call is
/// UB. Passing null is a no-op.
///
/// # Safety
/// If `b` is non-null, it must point to a builder previously returned
/// by [`mangle_val_builder_new`] that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_builder_free(b: *mut MangleValBuilder) {
    if b.is_null() {
        return;
    }
    // SAFETY: caller's contract guarantees a live, not-yet-freed
    // pointer; Drop will free each owned Value.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(b) });
    }));
}

// ---- Scalar builders ----------------------------------------------------

/// Build a `Null` value.
///
/// # Safety
/// `b` must be a live builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_null(b: *mut MangleValBuilder) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    // SAFETY: `b` non-null and live per the contract.
    unsafe { (*b).push(Value::Null) }
}

/// Build a `Number` value from an `i64`.
///
/// # Safety
/// `b` must be a live builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_i64(
    b: *mut MangleValBuilder,
    n: i64,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    unsafe { (*b).push(Value::Number(n)) }
}

/// Build a `Float` value from an `f64`.
///
/// # Safety
/// `b` must be a live builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_f64(
    b: *mut MangleValBuilder,
    f: f64,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    unsafe { (*b).push(Value::Float(f)) }
}

/// Build a `Time` value from i64 nanoseconds since the Unix epoch.
///
/// # Safety
/// `b` must be a live builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_time_ns(
    b: *mut MangleValBuilder,
    nanos: i64,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    unsafe { (*b).push(Value::Time(nanos)) }
}

/// Build a `Duration` value from i64 nanoseconds.
///
/// # Safety
/// `b` must be a live builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_duration_ns(
    b: *mut MangleValBuilder,
    nanos: i64,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    unsafe { (*b).push(Value::Duration(nanos)) }
}

/// Build a `String` value from a UTF-8 byte slice. Returns null and
/// sets last_error on invalid UTF-8.
///
/// # Safety
/// `b` must be live. `s` must point to `len` readable bytes (or be null
/// with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_string(
    b: *mut MangleValBuilder,
    s: *const u8,
    len: usize,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    let slice = match read_bytes(s, len) {
        Some(slice) => slice,
        None => {
            set_error_msg("mangle_val_build_string: null pointer with nonzero length");
            return std::ptr::null();
        }
    };
    let owned = match std::str::from_utf8(slice) {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_error_msg(format!("mangle_val_build_string: invalid UTF-8: {e}"));
            return std::ptr::null();
        }
    };
    unsafe { (*b).push(Value::String(owned)) }
}

/// Build a `Name` value from a UTF-8 byte slice.
///
/// Mangle name constants must start with `/`; otherwise this returns
/// null and populates `last_error`. (Matches mangle-py's `PyName::new`
/// contract.)
///
/// # Safety
/// `b` must be live. `s` must point to `len` readable bytes (or be null
/// with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_name(
    b: *mut MangleValBuilder,
    s: *const u8,
    len: usize,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    let slice = match read_bytes(s, len) {
        Some(slice) => slice,
        None => {
            set_error_msg("mangle_val_build_name: null pointer with nonzero length");
            return std::ptr::null();
        }
    };
    let name = match std::str::from_utf8(slice) {
        Ok(s) => s,
        Err(e) => {
            set_error_msg(format!("mangle_val_build_name: invalid UTF-8: {e}"));
            return std::ptr::null();
        }
    };
    if !name.starts_with('/') {
        set_error_msg("mangle_val_build_name: name constants must start with '/'");
        return std::ptr::null();
    }
    unsafe { (*b).push(Value::Name(name.to_string())) }
}

/// Build a compound value (`List`, `Pair`, `Map`, or `Struct`) by
/// cloning the given element handles into a new `Value::Compound`.
///
/// `subkind` selects the compound kind via the `MANGLE_COMPOUND_*`
/// constants. For `Map` and `Struct`, `elems` is a flat `[k0, v0, k1,
/// v1, ...]` sequence and `n` is the total entry count (so `n` must be
/// even).
///
/// Element handles must point to values living in the same builder (or
/// in any other producer with stable lifetime through this call); they
/// are cloned, not aliased, so the originals can be freed independently
/// of the resulting compound.
///
/// Returns null and sets last_error on bad subkind, bad element
/// pointer, or odd `n` for map/struct.
///
/// # Safety
/// `b` must be live. `elems` must point to `n` readable `*const MangleVal`
/// entries; each non-null entry must reference a live value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_build_compound(
    b: *mut MangleValBuilder,
    subkind: i32,
    elems: *const *const MangleVal,
    n: usize,
) -> *const MangleVal {
    if b.is_null() {
        return std::ptr::null();
    }
    let kind = match compound_subkind_from_i32(subkind) {
        Some(k) => k,
        None => {
            set_error_msg(format!(
                "mangle_val_build_compound: invalid subkind {subkind}"
            ));
            return std::ptr::null();
        }
    };
    if n > 0 && elems.is_null() {
        set_error_msg("mangle_val_build_compound: elems is null but n > 0");
        return std::ptr::null();
    }
    if matches!(kind, CompoundKind::Map | CompoundKind::Struct) && !n.is_multiple_of(2) {
        set_error_msg(format!(
            "mangle_val_build_compound: map/struct requires an even element count, got {n}"
        ));
        return std::ptr::null();
    }
    let mut values: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: caller guarantees elems has n entries.
        let p = unsafe { *elems.add(i) };
        if p.is_null() {
            set_error_msg(format!("mangle_val_build_compound: elems[{i}] is null"));
            return std::ptr::null();
        }
        // SAFETY: caller guarantees each non-null element is a live value.
        values.push(unsafe { (*p).clone() });
    }
    unsafe { (*b).push(Value::Compound(kind, values)) }
}

/// Helper: read `len` bytes from `s` into a borrowed slice. Returns
/// `None` if `s` is null and `len != 0`; returns an empty slice if
/// `len == 0` regardless of `s`.
fn read_bytes<'a>(s: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if s.is_null() {
        return None;
    }
    // SAFETY: precondition documented at each caller.
    Some(unsafe { std::slice::from_raw_parts(s, len) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{MangleBuffer, mangle_buffer_free};
    use crate::error::take_error;
    use crate::value::{
        MANGLE_COMPOUND_LIST, MANGLE_COMPOUND_MAP, MANGLE_COMPOUND_PAIR, MANGLE_COMPOUND_STRUCT,
        MANGLE_VAL_COMPOUND, MANGLE_VAL_DURATION, MANGLE_VAL_FLOAT, MANGLE_VAL_NAME,
        MANGLE_VAL_NULL, MANGLE_VAL_NUMBER, MANGLE_VAL_STRING, MANGLE_VAL_TIME, mangle_val_as_f64,
        mangle_val_as_i64, mangle_val_as_str, mangle_val_compound_get, mangle_val_compound_kind,
        mangle_val_compound_kv, mangle_val_compound_len, mangle_val_kind,
    };
    use crate::{MANGLE_ERR_INVALID_ARG, MANGLE_OK};
    use std::ptr;

    fn new_builder() -> *mut MangleValBuilder {
        unsafe { mangle_val_builder_new() }
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
    fn null_roundtrip() {
        let b = new_builder();
        let v = unsafe { mangle_val_build_null(b) };
        assert!(!v.is_null());
        assert_eq!(unsafe { mangle_val_kind(v) }, MANGLE_VAL_NULL);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn i64_roundtrip() {
        let b = new_builder();
        let v = unsafe { mangle_val_build_i64(b, -123) };
        assert_eq!(unsafe { mangle_val_kind(v) }, MANGLE_VAL_NUMBER);
        let mut out = 0_i64;
        assert_eq!(unsafe { mangle_val_as_i64(v, &mut out) }, MANGLE_OK);
        assert_eq!(out, -123);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn f64_roundtrip() {
        let b = new_builder();
        let v = unsafe { mangle_val_build_f64(b, 4.5) };
        assert_eq!(unsafe { mangle_val_kind(v) }, MANGLE_VAL_FLOAT);
        let mut out = 0.0_f64;
        assert_eq!(unsafe { mangle_val_as_f64(v, &mut out) }, MANGLE_OK);
        assert_eq!(out, 4.5);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn time_and_duration_roundtrip() {
        let b = new_builder();
        let t = unsafe { mangle_val_build_time_ns(b, 1_000_000_000) };
        let d = unsafe { mangle_val_build_duration_ns(b, 5_000) };
        assert_eq!(unsafe { mangle_val_kind(t) }, MANGLE_VAL_TIME);
        assert_eq!(unsafe { mangle_val_kind(d) }, MANGLE_VAL_DURATION);
        let mut out = 0_i64;
        assert_eq!(unsafe { mangle_val_as_i64(t, &mut out) }, MANGLE_OK);
        assert_eq!(out, 1_000_000_000);
        assert_eq!(unsafe { mangle_val_as_i64(d, &mut out) }, MANGLE_OK);
        assert_eq!(out, 5_000);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn string_roundtrip() {
        let b = new_builder();
        let s = "hello world";
        let v = unsafe { mangle_val_build_string(b, s.as_ptr(), s.len()) };
        assert_eq!(unsafe { mangle_val_kind(v) }, MANGLE_VAL_STRING);
        assert_eq!(read_str(v), s);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn string_empty_is_allowed() {
        let b = new_builder();
        let v = unsafe { mangle_val_build_string(b, ptr::null(), 0) };
        assert!(!v.is_null());
        assert_eq!(read_str(v), "");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn string_invalid_utf8_returns_null() {
        let b = new_builder();
        let bad: [u8; 3] = [0xff, 0xfe, 0xfd];
        let v = unsafe { mangle_val_build_string(b, bad.as_ptr(), bad.len()) };
        assert!(v.is_null());
        let err = take_error().expect("err set");
        assert!(err.contains("UTF-8"), "got: {err}");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn name_roundtrip() {
        let b = new_builder();
        let s = "/role/admin";
        let v = unsafe { mangle_val_build_name(b, s.as_ptr(), s.len()) };
        assert_eq!(unsafe { mangle_val_kind(v) }, MANGLE_VAL_NAME);
        assert_eq!(read_str(v), s, "as_str preserves the leading slash");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn name_without_slash_returns_null() {
        let b = new_builder();
        let s = "admin";
        let v = unsafe { mangle_val_build_name(b, s.as_ptr(), s.len()) };
        assert!(v.is_null());
        let err = take_error().expect("err set");
        assert!(err.contains("'/'"), "got: {err}");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_list_roundtrip() {
        let b = new_builder();
        let a = unsafe { mangle_val_build_i64(b, 1) };
        let bb = unsafe { mangle_val_build_i64(b, 2) };
        let c = unsafe { mangle_val_build_i64(b, 3) };
        let elems = [a, bb, c];
        let list = unsafe {
            mangle_val_build_compound(b, MANGLE_COMPOUND_LIST, elems.as_ptr(), elems.len())
        };
        assert_eq!(unsafe { mangle_val_kind(list) }, MANGLE_VAL_COMPOUND);
        let mut subkind = -1_i32;
        unsafe { mangle_val_compound_kind(list, &mut subkind) };
        assert_eq!(subkind, MANGLE_COMPOUND_LIST);
        let mut len = 0_usize;
        unsafe { mangle_val_compound_len(list, &mut len) };
        assert_eq!(len, 3);
        for (i, want) in [1_i64, 2, 3].iter().enumerate() {
            let elem = unsafe { mangle_val_compound_get(list, i) };
            let mut got = 0_i64;
            unsafe { mangle_val_as_i64(elem, &mut got) };
            assert_eq!(got, *want);
        }
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_pair_roundtrip() {
        let b = new_builder();
        let k = unsafe {
            let n = "/k";
            mangle_val_build_name(b, n.as_ptr(), n.len())
        };
        let v = unsafe { mangle_val_build_i64(b, 99) };
        let elems = [k, v];
        let pair = unsafe {
            mangle_val_build_compound(b, MANGLE_COMPOUND_PAIR, elems.as_ptr(), elems.len())
        };
        let mut subkind = -1_i32;
        unsafe { mangle_val_compound_kind(pair, &mut subkind) };
        assert_eq!(subkind, MANGLE_COMPOUND_PAIR);
        let mut len = 0_usize;
        unsafe { mangle_val_compound_len(pair, &mut len) };
        assert_eq!(len, 2, "Pair uses linear element semantics, not kv");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_struct_roundtrip_via_kv() {
        let b = new_builder();
        // { /n: 1, /list: [10, 20] }
        let kn = unsafe {
            let s = "/n";
            mangle_val_build_name(b, s.as_ptr(), s.len())
        };
        let one = unsafe { mangle_val_build_i64(b, 1) };
        let klist = unsafe {
            let s = "/list";
            mangle_val_build_name(b, s.as_ptr(), s.len())
        };
        let ten = unsafe { mangle_val_build_i64(b, 10) };
        let twenty = unsafe { mangle_val_build_i64(b, 20) };
        let inner_elems = [ten, twenty];
        let inner = unsafe {
            mangle_val_build_compound(
                b,
                MANGLE_COMPOUND_LIST,
                inner_elems.as_ptr(),
                inner_elems.len(),
            )
        };
        let elems = [kn, one, klist, inner];
        let s = unsafe {
            mangle_val_build_compound(b, MANGLE_COMPOUND_STRUCT, elems.as_ptr(), elems.len())
        };

        let mut len = 0_usize;
        unsafe { mangle_val_compound_len(s, &mut len) };
        assert_eq!(len, 2, "struct len is pair count");

        let mut k = ptr::null::<MangleVal>();
        let mut v = ptr::null::<MangleVal>();
        unsafe { mangle_val_compound_kv(s, 0, &mut k, &mut v) };
        assert_eq!(unsafe { mangle_val_kind(k) }, MANGLE_VAL_NAME);
        assert_eq!(read_str(k), "/n");
        let mut n = 0_i64;
        unsafe { mangle_val_as_i64(v, &mut n) };
        assert_eq!(n, 1);

        unsafe { mangle_val_compound_kv(s, 1, &mut k, &mut v) };
        assert_eq!(read_str(k), "/list");
        let mut sub_len = 0_usize;
        unsafe { mangle_val_compound_len(v, &mut sub_len) };
        assert_eq!(sub_len, 2);

        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_map_odd_n_returns_null() {
        let b = new_builder();
        let a = unsafe { mangle_val_build_i64(b, 1) };
        let elems = [a];
        let v = unsafe {
            mangle_val_build_compound(b, MANGLE_COMPOUND_MAP, elems.as_ptr(), elems.len())
        };
        assert!(v.is_null());
        let err = take_error().expect("err set");
        assert!(err.contains("even"), "got: {err}");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_bad_subkind_returns_null() {
        let b = new_builder();
        let v = unsafe { mangle_val_build_compound(b, 99, ptr::null(), 0) };
        assert!(v.is_null());
        let err = take_error().expect("err set");
        assert!(err.contains("subkind"), "got: {err}");
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn compound_null_elem_returns_null() {
        let b = new_builder();
        let a = unsafe { mangle_val_build_i64(b, 1) };
        let elems: [*const MangleVal; 2] = [a, ptr::null()];
        let v = unsafe {
            mangle_val_build_compound(b, MANGLE_COMPOUND_LIST, elems.as_ptr(), elems.len())
        };
        assert!(v.is_null());
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn handles_remain_stable_across_many_pushes() {
        // Push many values, then re-read the first one — its handle
        // must still be valid because each Value is on the heap, not
        // in a contiguous vec.
        let b = new_builder();
        let first = unsafe { mangle_val_build_i64(b, 42) };
        for i in 0..10_000_i64 {
            let _ = unsafe { mangle_val_build_i64(b, i) };
        }
        let mut out = 0_i64;
        assert_eq!(unsafe { mangle_val_as_i64(first, &mut out) }, MANGLE_OK);
        assert_eq!(out, 42);
        unsafe { mangle_val_builder_free(b) };
    }

    #[test]
    fn null_builder_returns_null_handle() {
        let v = unsafe { mangle_val_build_i64(ptr::null_mut(), 1) };
        assert!(v.is_null());
    }

    #[test]
    fn builder_free_null_is_noop() {
        unsafe { mangle_val_builder_free(ptr::null_mut()) };
    }

    #[test]
    fn val_kind_minus_one_for_null() {
        // Sanity tie-in with the value module: null handle → -1.
        assert_eq!(unsafe { mangle_val_kind(ptr::null()) }, -1);
        // And our error sentinel for accessors.
        assert_eq!(
            unsafe { mangle_val_as_i64(ptr::null(), ptr::null_mut()) },
            MANGLE_ERR_INVALID_ARG
        );
    }
}
