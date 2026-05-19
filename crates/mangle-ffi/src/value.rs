//! Opaque value handles + accessors crossing the FFI boundary.
//!
//! [`MangleVal`] is a stable opaque type — `*const MangleVal` is the
//! caller-side view, and the implementation reads them as
//! `*const mangle_common::Value`. The internal layout is intentionally
//! hidden so it can change without breaking the ABI; consumers can only
//! observe values through the accessor functions in this module.
//!
//! Lifetime: a `*const MangleVal` is borrowed from whatever produced it
//! (a [`crate::builder::MangleValBuilder`] today, a cursor row buffer
//! starting in M4). Using a handle after the producer is freed is UB.
//! The cbindgen header documents this per-source.
//!
//! The accessor functions are all read-only and shouldn't panic in
//! normal use, but they go through `catch_unwind` anyway so a dangling
//! pointer (which would panic on deref-pattern-mismatch) returns a
//! sentinel rather than unwinding across the FFI boundary.

use mangle_common::{CompoundKind, Value};

/// Render a single `Value` as JSON, in the lossy-but-unambiguous shape
/// used by snapshot endpoints (`mangle_facts_snapshot`,
/// `mangle_derivation_tree`).
///
/// Scalars become JSON primitives. Non-primitives become tagged
/// objects so consumers can distinguish `String("/x")` from
/// `Name("/x")`, ints from times/durations, etc:
///
/// - `Name(s)` → `{ "name": s }`
/// - `Time(ns)` → `{ "time_ns": ns }`
/// - `Duration(ns)` → `{ "duration_ns": ns }`
/// - `Compound(kind, elems)` → `{ "compound": "list"|"pair"|"map"|
///   "struct", "elems": [...] }` with elements recursively encoded.
///
/// Round-trip is NOT supported via this format — that's what the
/// batch-encode (`.mgr`) path is for. This shape is purely for
/// visualization-layer consumption.
pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Number(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::json!(s),
        Value::Name(n) => serde_json::json!({ "name": n }),
        Value::Time(ns) => serde_json::json!({ "time_ns": ns }),
        Value::Duration(ns) => serde_json::json!({ "duration_ns": ns }),
        Value::Compound(kind, elems) => {
            let subkind = match kind {
                CompoundKind::List => "list",
                CompoundKind::Pair => "pair",
                CompoundKind::Map => "map",
                CompoundKind::Struct => "struct",
            };
            let elems: Vec<serde_json::Value> = elems.iter().map(value_to_json).collect();
            serde_json::json!({ "compound": subkind, "elems": elems })
        }
    }
}

use crate::error::set_error_msg;
use crate::{MANGLE_ERR, MANGLE_ERR_INVALID_ARG, MANGLE_OK};

/// Opaque value handle. Pointer-only; the layout is internal.
///
/// We expose `MangleVal` as a type alias for `mangle_common::Value` to
/// keep the implementation simple, but the ABI contract is that
/// consumers treat `*const MangleVal` as opaque — only the accessor
/// functions here are stable.
pub type MangleVal = Value;

// ---- Kind tags (returned by mangle_val_kind) -----------------------------

/// Value kind: `Null`.
pub const MANGLE_VAL_NULL: i32 = 0;
/// Value kind: `Number(i64)`.
pub const MANGLE_VAL_NUMBER: i32 = 1;
/// Value kind: `Float(f64)`.
pub const MANGLE_VAL_FLOAT: i32 = 2;
/// Value kind: `String`.
pub const MANGLE_VAL_STRING: i32 = 3;
/// Value kind: `Name` (a `/`-prefixed identifier).
pub const MANGLE_VAL_NAME: i32 = 4;
/// Value kind: `Time` (i64 nanoseconds since Unix epoch).
pub const MANGLE_VAL_TIME: i32 = 5;
/// Value kind: `Duration` (i64 nanoseconds).
pub const MANGLE_VAL_DURATION: i32 = 6;
/// Value kind: `Compound` (list, pair, map, or struct).
pub const MANGLE_VAL_COMPOUND: i32 = 7;

// ---- Compound subkinds ---------------------------------------------------

/// Compound subkind: ordered sequence.
pub const MANGLE_COMPOUND_LIST: i32 = 0;
/// Compound subkind: two-element pair.
pub const MANGLE_COMPOUND_PAIR: i32 = 1;
/// Compound subkind: keyed map (flat `[k0, v0, k1, v1, ...]`).
pub const MANGLE_COMPOUND_MAP: i32 = 2;
/// Compound subkind: struct (flat `[k0, v0, k1, v1, ...]` with name keys).
pub const MANGLE_COMPOUND_STRUCT: i32 = 3;

fn compound_subkind_to_i32(k: CompoundKind) -> i32 {
    match k {
        CompoundKind::List => MANGLE_COMPOUND_LIST,
        CompoundKind::Pair => MANGLE_COMPOUND_PAIR,
        CompoundKind::Map => MANGLE_COMPOUND_MAP,
        CompoundKind::Struct => MANGLE_COMPOUND_STRUCT,
    }
}

pub(crate) fn compound_subkind_from_i32(k: i32) -> Option<CompoundKind> {
    match k {
        MANGLE_COMPOUND_LIST => Some(CompoundKind::List),
        MANGLE_COMPOUND_PAIR => Some(CompoundKind::Pair),
        MANGLE_COMPOUND_MAP => Some(CompoundKind::Map),
        MANGLE_COMPOUND_STRUCT => Some(CompoundKind::Struct),
        _ => None,
    }
}

/// Wrap a read-only accessor body in `catch_unwind` so a dangling
/// pointer can't unwind across the FFI boundary. Returns `on_panic` if
/// the body panics.
fn read_boundary<F, T>(on_panic: T, body: F) -> T
where
    F: FnOnce() -> T + std::panic::UnwindSafe,
{
    std::panic::catch_unwind(body).unwrap_or(on_panic)
}

/// Report the kind tag of a value.
///
/// Returns one of the `MANGLE_VAL_*` constants. Returns `-1` if `v` is
/// null or accessing it panics.
///
/// # Safety
/// If `v` is non-null, it must point to a live `MangleVal` borrowed
/// from a producer (builder or cursor row) that has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_kind(v: *const MangleVal) -> i32 {
    if v.is_null() {
        return -1;
    }
    read_boundary(-1, || match unsafe { &*v } {
        Value::Null => MANGLE_VAL_NULL,
        Value::Number(_) => MANGLE_VAL_NUMBER,
        Value::Float(_) => MANGLE_VAL_FLOAT,
        Value::String(_) => MANGLE_VAL_STRING,
        Value::Name(_) => MANGLE_VAL_NAME,
        Value::Time(_) => MANGLE_VAL_TIME,
        Value::Duration(_) => MANGLE_VAL_DURATION,
        Value::Compound(_, _) => MANGLE_VAL_COMPOUND,
    })
}

/// Extract an `i64` from a `Number`, `Time`, or `Duration` value.
///
/// Returns [`MANGLE_OK`] on success. Returns [`MANGLE_ERR_INVALID_ARG`]
/// for null inputs and [`MANGLE_ERR`] for a wrong-kind value (with the
/// last_error slot populated).
///
/// # Safety
/// `v` and `out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_as_i64(v: *const MangleVal, out: *mut i64) -> i32 {
    if v.is_null() || out.is_null() {
        set_error_msg("mangle_val_as_i64: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::Number(n) | Value::Time(n) | Value::Duration(n) => {
            // SAFETY: out non-null per the precondition.
            unsafe { *out = *n };
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_as_i64: value is not Number/Time/Duration");
            MANGLE_ERR
        }
    })
}

/// Extract an `f64` from a `Float` value.
///
/// # Safety
/// `v` and `out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_as_f64(v: *const MangleVal, out: *mut f64) -> i32 {
    if v.is_null() || out.is_null() {
        set_error_msg("mangle_val_as_f64: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::Float(f) => {
            unsafe { *out = *f };
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_as_f64: value is not Float");
            MANGLE_ERR
        }
    })
}

/// Copy the textual content of a `String` or `Name` value into `out`.
///
/// For a `Name`, the leading `/` is **kept** — consumers that want it
/// stripped should slice the buffer after the call. (Keeping it
/// preserves the round-trip property: `build_name(as_str(name))` is the
/// identity.)
///
/// # Safety
/// `v` and `out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_as_str(
    v: *const MangleVal,
    out: *mut crate::buffer::MangleBuffer,
) -> i32 {
    if v.is_null() || out.is_null() {
        set_error_msg("mangle_val_as_str: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::String(s) | Value::Name(s) => {
            // SAFETY: out non-null per the precondition.
            unsafe { crate::buffer::write_buffer(out, s.as_bytes().to_vec()) };
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_as_str: value is not String/Name");
            MANGLE_ERR
        }
    })
}

/// Report the compound subkind of a compound value.
///
/// # Safety
/// `v` and `subkind_out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_compound_kind(
    v: *const MangleVal,
    subkind_out: *mut i32,
) -> i32 {
    if v.is_null() || subkind_out.is_null() {
        set_error_msg("mangle_val_compound_kind: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::Compound(k, _) => {
            unsafe { *subkind_out = compound_subkind_to_i32(*k) };
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_compound_kind: value is not a compound");
            MANGLE_ERR
        }
    })
}

/// Report the number of items (for `List`/`Pair`) or pairs (for
/// `Map`/`Struct`) in a compound value.
///
/// For maps and structs, the underlying flat vector has `2*N` entries
/// for `N` pairs; this function returns `N`, not `2*N`.
///
/// # Safety
/// `v` and `out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_compound_len(v: *const MangleVal, out: *mut usize) -> i32 {
    if v.is_null() || out.is_null() {
        set_error_msg("mangle_val_compound_len: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::Compound(k, vec) => {
            let len = match k {
                CompoundKind::Map | CompoundKind::Struct => vec.len() / 2,
                CompoundKind::List | CompoundKind::Pair => vec.len(),
            };
            unsafe { *out = len };
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_compound_len: value is not a compound");
            MANGLE_ERR
        }
    })
}

/// Borrowed handle to a compound element at the given linear index.
///
/// Use this for `List`/`Pair` values. For `Map`/`Struct`, use
/// [`mangle_val_compound_kv`] instead — the indexing semantics differ
/// (pair index vs. flat index).
///
/// Returns `null` if `v` is null, not a compound, or the index is out
/// of range. The returned handle borrows from the same producer as
/// `v`; do not outlive it.
///
/// # Safety
/// `v` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_compound_get(
    v: *const MangleVal,
    index: usize,
) -> *const MangleVal {
    if v.is_null() {
        return std::ptr::null();
    }
    read_boundary(std::ptr::null(), || match unsafe { &*v } {
        Value::Compound(_, vec) if index < vec.len() => &vec[index] as *const MangleVal,
        _ => std::ptr::null(),
    })
}

/// Borrowed handles to the key and value at a given pair index in a
/// `Map` or `Struct` value.
///
/// `pair_index` is in `0..N` where `N` is `mangle_val_compound_len`.
/// The underlying flat vector positions are `2 * pair_index` (key) and
/// `2 * pair_index + 1` (value).
///
/// # Safety
/// `v`, `key_out`, and `val_out` must be non-null and live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_val_compound_kv(
    v: *const MangleVal,
    pair_index: usize,
    key_out: *mut *const MangleVal,
    val_out: *mut *const MangleVal,
) -> i32 {
    if v.is_null() || key_out.is_null() || val_out.is_null() {
        set_error_msg("mangle_val_compound_kv: null argument");
        return MANGLE_ERR_INVALID_ARG;
    }
    read_boundary(MANGLE_ERR, || match unsafe { &*v } {
        Value::Compound(CompoundKind::Map | CompoundKind::Struct, vec) => {
            let n_pairs = vec.len() / 2;
            if pair_index >= n_pairs {
                set_error_msg("mangle_val_compound_kv: pair_index out of range");
                return MANGLE_ERR;
            }
            let k = pair_index * 2;
            unsafe {
                *key_out = &vec[k] as *const MangleVal;
                *val_out = &vec[k + 1] as *const MangleVal;
            }
            MANGLE_OK
        }
        _ => {
            set_error_msg("mangle_val_compound_kv: value is not a map/struct");
            MANGLE_ERR
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{MangleBuffer, mangle_buffer_free};
    use std::ptr;

    fn make(v: Value) -> Box<Value> {
        Box::new(v)
    }

    #[test]
    fn kind_each_variant() {
        let cases = [
            (Value::Null, MANGLE_VAL_NULL),
            (Value::Number(7), MANGLE_VAL_NUMBER),
            (Value::Float(1.5), MANGLE_VAL_FLOAT),
            (Value::String("x".into()), MANGLE_VAL_STRING),
            (Value::Name("/x".into()), MANGLE_VAL_NAME),
            (Value::Time(123), MANGLE_VAL_TIME),
            (Value::Duration(456), MANGLE_VAL_DURATION),
            (
                Value::Compound(CompoundKind::List, vec![]),
                MANGLE_VAL_COMPOUND,
            ),
        ];
        for (v, want) in cases {
            let b = make(v);
            assert_eq!(unsafe { mangle_val_kind(&*b) }, want);
        }
    }

    #[test]
    fn kind_null_returns_minus_one() {
        assert_eq!(unsafe { mangle_val_kind(ptr::null()) }, -1);
    }

    #[test]
    fn as_i64_accepts_number_time_duration() {
        for v in [Value::Number(42), Value::Time(42), Value::Duration(42)] {
            let b = make(v);
            let mut out = 0_i64;
            assert_eq!(unsafe { mangle_val_as_i64(&*b, &mut out) }, MANGLE_OK);
            assert_eq!(out, 42);
        }
    }

    #[test]
    fn as_i64_rejects_non_integer() {
        let b = make(Value::String("nope".into()));
        let mut out = 0_i64;
        assert_eq!(unsafe { mangle_val_as_i64(&*b, &mut out) }, MANGLE_ERR);
    }

    #[test]
    fn as_f64_accepts_float() {
        let b = make(Value::Float(2.5));
        let mut out = 0.0_f64;
        assert_eq!(unsafe { mangle_val_as_f64(&*b, &mut out) }, MANGLE_OK);
        assert_eq!(out, 2.5);
    }

    #[test]
    fn as_str_string_and_name() {
        for v in [Value::String("hello".into()), Value::Name("/hello".into())] {
            let b = make(v.clone());
            let mut buf = MangleBuffer::empty();
            assert_eq!(unsafe { mangle_val_as_str(&*b, &mut buf) }, MANGLE_OK);
            let s = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
            let s = std::str::from_utf8(s).unwrap();
            match &v {
                Value::String(orig) | Value::Name(orig) => assert_eq!(s, orig),
                _ => unreachable!(),
            }
            unsafe { mangle_buffer_free(&mut buf) };
        }
    }

    #[test]
    fn compound_list_walk() {
        let v = Value::Compound(
            CompoundKind::List,
            vec![Value::Number(1), Value::Number(2), Value::Number(3)],
        );
        let b = make(v);
        let mut subkind = -1_i32;
        assert_eq!(
            unsafe { mangle_val_compound_kind(&*b, &mut subkind) },
            MANGLE_OK
        );
        assert_eq!(subkind, MANGLE_COMPOUND_LIST);

        let mut len = 0_usize;
        assert_eq!(unsafe { mangle_val_compound_len(&*b, &mut len) }, MANGLE_OK);
        assert_eq!(len, 3);

        for (i, want) in [1, 2, 3].iter().enumerate() {
            let elem = unsafe { mangle_val_compound_get(&*b, i) };
            assert!(!elem.is_null());
            let mut n = 0_i64;
            assert_eq!(unsafe { mangle_val_as_i64(elem, &mut n) }, MANGLE_OK);
            assert_eq!(n, *want);
        }

        // Out-of-range index → null.
        let out_of_range = unsafe { mangle_val_compound_get(&*b, 99) };
        assert!(out_of_range.is_null());
    }

    #[test]
    fn compound_struct_walk_kv() {
        // Struct { "n": 1, "list": [10, 20] } — keys are Name values per
        // the convention.
        let v = Value::Compound(
            CompoundKind::Struct,
            vec![
                Value::Name("/n".into()),
                Value::Number(1),
                Value::Name("/list".into()),
                Value::Compound(
                    CompoundKind::List,
                    vec![Value::Number(10), Value::Number(20)],
                ),
            ],
        );
        let b = make(v);
        let mut len = 0_usize;
        assert_eq!(unsafe { mangle_val_compound_len(&*b, &mut len) }, MANGLE_OK);
        assert_eq!(len, 2, "pair count is 2, not 4");

        // Pair 0: name=/n, value=Number(1)
        let mut k = ptr::null::<MangleVal>();
        let mut val = ptr::null::<MangleVal>();
        assert_eq!(
            unsafe { mangle_val_compound_kv(&*b, 0, &mut k, &mut val) },
            MANGLE_OK
        );
        assert_eq!(unsafe { mangle_val_kind(k) }, MANGLE_VAL_NAME);
        let mut n = 0_i64;
        assert_eq!(unsafe { mangle_val_as_i64(val, &mut n) }, MANGLE_OK);
        assert_eq!(n, 1);

        // Pair 1: name=/list, value=Compound list of 2 numbers.
        assert_eq!(
            unsafe { mangle_val_compound_kv(&*b, 1, &mut k, &mut val) },
            MANGLE_OK
        );
        assert_eq!(unsafe { mangle_val_kind(val) }, MANGLE_VAL_COMPOUND);
        let mut inner_len = 0_usize;
        assert_eq!(
            unsafe { mangle_val_compound_len(val, &mut inner_len) },
            MANGLE_OK
        );
        assert_eq!(inner_len, 2);

        // Out-of-range pair_index.
        assert_eq!(
            unsafe { mangle_val_compound_kv(&*b, 99, &mut k, &mut val) },
            MANGLE_ERR
        );
    }

    #[test]
    fn compound_kv_on_list_returns_error() {
        let v = Value::Compound(CompoundKind::List, vec![Value::Number(1)]);
        let b = make(v);
        let mut k = ptr::null::<MangleVal>();
        let mut val = ptr::null::<MangleVal>();
        assert_eq!(
            unsafe { mangle_val_compound_kv(&*b, 0, &mut k, &mut val) },
            MANGLE_ERR
        );
    }
}
