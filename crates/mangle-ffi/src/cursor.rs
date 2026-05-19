//! Query cursors.
//!
//! A [`MangleCursor`] is the read-side handle returned by
//! [`mangle_query`]. It holds:
//!   - a borrowed pointer back to the engine (used only to read the
//!     `generation` field on each call);
//!   - the engine's generation at the moment the cursor was opened;
//!   - pre-materialized, pre-filtered result tuples;
//!   - a cursor index + the most recent row (for column accessors).
//!
//! Rows are **materialized up front** (collected into a `Vec<Vec<Value>>`
//! inside `with_interp`). The cursor is therefore independent of the
//! engine's internal data for memory-safety purposes: freeing the
//! engine, then freeing the cursor, is safe. What is *not* safe is
//! calling `mangle_cursor_next` after the engine has been freed — that
//! dereferences the dangling engine pointer to read the generation
//! counter, which is undefined behavior. Per the documented contract,
//! consumers must call `mangle_cursor_next` only while the engine is
//! alive; `mangle_cursor_free` may be called in any order.
//!
//! Streaming (avoiding the up-front collect) would require holding a
//! borrow into the engine's Store for the cursor's lifetime, which
//! conflicts with the ouroboros engine layout. M7's batch-encode path
//! exists for the case where materialization would be too expensive;
//! streaming is a possible later optimization.

use crate::engine::MangleEngine;
use crate::error::{panic_boundary, set_error_msg};
use crate::query::{filter_tuples, parse_query_lenient};
use crate::value::MangleVal;
use crate::{
    MANGLE_ERR, MANGLE_ERR_CURSOR_INVALIDATED, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES,
    MANGLE_ERR_PARSE, MANGLE_ERR_UNKNOWN_RELATION, MANGLE_OK,
};
use mangle_common::Value;

/// Query result cursor.
pub struct MangleCursor {
    /// Engine that produced this cursor. Used only to read
    /// `engine.generation` in `cursor_next`. The cursor never
    /// dereferences any other engine field.
    ///
    /// SAFETY: must point to a live engine when `cursor_next` is
    /// called. `cursor_free` does not deref this pointer.
    engine: *const MangleEngine,
    /// Engine generation at the moment this cursor was opened.
    /// Mismatch with the current generation invalidates the cursor.
    generation_at_open: u64,
    /// Pre-materialized, pre-filtered rows.
    rows: Vec<Vec<Value>>,
    /// Index of the next row to yield.
    index: usize,
    /// The row most recently produced by `cursor_next`. `None` before
    /// the first call and after end-of-stream. Column accessors read
    /// from this.
    current_row: Option<Vec<Value>>,
}

/// Open a cursor over the tuples matching `query`.
///
/// `query` is a Mangle atom: a bare predicate name like `reachable`,
/// or `pred(arg1, ...)` with constants used as equality filters and
/// uppercase identifiers acting as wildcards. See
/// [`crate::query::parse_query_lenient`] for the exact syntax.
///
/// Returns [`MANGLE_OK`] on success and writes the cursor pointer to
/// `*out`. Returns [`MANGLE_ERR_NO_RULES`] if the engine has no
/// program loaded, [`MANGLE_ERR_PARSE`] for a malformed query, or
/// [`MANGLE_ERR`] if the named relation does not exist in the store.
///
/// The cursor must be released with [`mangle_cursor_free`] (which is
/// safe to call in any order with respect to engine free). Calling
/// [`mangle_cursor_next`] requires the engine to still be alive.
///
/// # Safety
/// `engine` must be a live handle. `query` must point to `query_len`
/// readable UTF-8 bytes (or be null with `query_len == 0`). `out` must
/// be non-null and point to writable storage for a `*mut MangleCursor`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_query(
    engine: *mut MangleEngine,
    query: *const u8,
    query_len: usize,
    out: *mut *mut MangleCursor,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_query: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let query_slice = if query_len == 0 {
            &[][..]
        } else if query.is_null() {
            set_error_msg("mangle_query: query is null but length is nonzero");
            return MANGLE_ERR_INVALID_ARG;
        } else {
            // SAFETY: caller's contract.
            unsafe { std::slice::from_raw_parts(query, query_len) }
        };
        let query_str = match std::str::from_utf8(query_slice) {
            Ok(s) => s,
            Err(e) => {
                set_error_msg(format!("mangle_query: invalid UTF-8: {e}"));
                return MANGLE_ERR_INVALID_ARG;
            }
        };
        let parsed = match parse_query_lenient(query_str) {
            Ok(p) => p,
            Err(e) => {
                set_error_msg(format!("mangle_query: {e:#}"));
                return MANGLE_ERR_PARSE;
            }
        };

        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        // Schema check (M8): catch typos before reaching the store,
        // which would otherwise silently return an empty iterator.
        match eng.schema() {
            Some(s) if !s.knows(&parsed.predicate) => {
                set_error_msg(format!(
                    "mangle_query: unknown relation `{}`",
                    parsed.predicate
                ));
                return MANGLE_ERR_UNKNOWN_RELATION;
            }
            _ => {}
        }
        let materialized = match eng.materialize_relation(&parsed.predicate) {
            Ok(Some(rows)) => rows,
            Ok(None) => {
                set_error_msg("mangle_query: engine has no rules loaded");
                return MANGLE_ERR_NO_RULES;
            }
            Err(e) => {
                set_error_msg(format!("mangle_query: {e:#}"));
                return MANGLE_ERR;
            }
        };
        let rows = filter_tuples(materialized, &parsed);

        let cursor = Box::new(MangleCursor {
            engine: engine as *const MangleEngine,
            generation_at_open: eng.generation,
            rows,
            index: 0,
            current_row: None,
        });
        // SAFETY: out non-null per the precondition.
        unsafe { *out = Box::into_raw(cursor) };
        MANGLE_OK
    })
}

/// Advance the cursor to the next row.
///
/// Returns:
///   - [`MANGLE_OK`] (0) when a row was produced; access columns via
///     [`mangle_cursor_arity`] and [`mangle_cursor_col`].
///   - `1` when the cursor has reached end-of-stream. Further calls
///     also return `1`.
///   - [`MANGLE_ERR_CURSOR_INVALIDATED`] when the engine has been
///     reloaded or poisoned since the cursor was opened. The cursor
///     handle is still safe to free but yields no more rows.
///   - [`MANGLE_ERR_INVALID_ARG`] when `cursor` is null.
///
/// # Safety
/// `cursor` must be a live cursor. The engine that produced it must be
/// alive at the time of the call (it is dereferenced to read the
/// generation counter); calling this after `mangle_engine_free` on
/// the producing engine is undefined behavior.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_cursor_next(cursor: *mut MangleCursor) -> i32 {
    if cursor.is_null() {
        set_error_msg("mangle_cursor_next: cursor pointer is null");
        return MANGLE_ERR_INVALID_ARG;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: cursor non-null per the precondition.
        let c = unsafe { &mut *cursor };
        if c.engine.is_null() {
            set_error_msg("mangle_cursor_next: cursor has no associated engine");
            return MANGLE_ERR_INVALID_ARG;
        }
        // SAFETY: caller's contract — engine is alive.
        let current_gen = unsafe { (*c.engine).generation };
        if current_gen != c.generation_at_open {
            set_error_msg(
                "mangle_cursor_next: engine was reloaded or panicked; cursor is invalidated",
            );
            c.current_row = None;
            return MANGLE_ERR_CURSOR_INVALIDATED;
        }
        if c.index >= c.rows.len() {
            c.current_row = None;
            return 1;
        }
        // Take by value (mem::take) so we don't have to clone the row.
        // We never revisit a row, so leaving an empty placeholder is fine.
        c.current_row = Some(std::mem::take(&mut c.rows[c.index]));
        c.index += 1;
        MANGLE_OK
    }));
    match result {
        Ok(rc) => rc,
        Err(payload) => {
            crate::error::set_error_from_panic(payload);
            crate::MANGLE_ERR_PANIC
        }
    }
}

/// Report the arity (column count) of the current row.
///
/// Returns the column count when a row is loaded (after a successful
/// [`mangle_cursor_next`]). Returns `0` when no row is loaded
/// (before first call or after end-of-stream). Returns `-1` if
/// `cursor` is null.
///
/// # Safety
/// `cursor` must be null or a live cursor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_cursor_arity(cursor: *mut MangleCursor) -> i32 {
    if cursor.is_null() {
        return -1;
    }
    // SAFETY: cursor non-null per the precondition.
    let c = unsafe { &*cursor };
    match &c.current_row {
        Some(row) => i32::try_from(row.len()).unwrap_or(i32::MAX),
        None => 0,
    }
}

/// Borrowed handle to the column at `col_idx` in the current row.
///
/// Returns null if `cursor` is null, no row is loaded, or the index is
/// out of range. The returned handle borrows from the cursor's
/// current-row buffer and is valid until the next call to
/// [`mangle_cursor_next`] or [`mangle_cursor_free`] on this cursor.
///
/// # Safety
/// `cursor` must be null or a live cursor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_cursor_col(
    cursor: *mut MangleCursor,
    col_idx: u32,
) -> *const MangleVal {
    if cursor.is_null() {
        return std::ptr::null();
    }
    // SAFETY: cursor non-null per the precondition.
    let c = unsafe { &*cursor };
    let row = match &c.current_row {
        Some(r) => r,
        None => return std::ptr::null(),
    };
    let idx = col_idx as usize;
    if idx >= row.len() {
        return std::ptr::null();
    }
    &row[idx] as *const MangleVal
}

/// Release a cursor.
///
/// Safe to call on null. Safe to call regardless of the producing
/// engine's state (the cursor's row data is owned, not borrowed; this
/// function does not dereference the stored engine pointer).
///
/// # Safety
/// If `cursor` is non-null, it must point to a cursor previously
/// returned by [`mangle_query`] that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_cursor_free(cursor: *mut MangleCursor) {
    if cursor.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: caller's contract.
        drop(unsafe { Box::from_raw(cursor) });
    }));
}
