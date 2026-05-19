//! Engine handle and lifecycle.
//!
//! [`MangleEngine`] is the top-level opaque handle the C ABI hands out
//! via [`mangle_engine_new`]. The engine carries:
//!   - configuration (the `enable_provenance` flag captured at
//!     construction);
//!   - lifecycle state (`poisoned`, `generation`);
//!   - optionally, a `ProgramInner` holding the compiled IR + a running
//!     interpreter. The ouroboros pattern bundles the `Arena`, the
//!     `CompiledIr<'arena>`, and the `Interpreter<'compiled>` into one
//!     allocation so the lifetime soup never crosses an FFI boundary.
//!
//! State machine:
//!   - **Fresh**: `inner = None`. Just constructed, no rules loaded.
//!     Queries / snapshots return `MANGLE_ERR_NO_RULES`.
//!   - **Loaded**: `inner = Some(_)`. Set by `mangle_load_rules`.
//!   - **Poisoned**: `poisoned = true` (one-way). Set by the
//!     `panic_boundary!` macro on caught panic. All non-free operations
//!     return `MANGLE_ERR_PANIC`.
//!
//! `generation` is a monotonic counter bumped on every `load_rules` and
//! on any poisoning panic. Cursors (M4) stamp the value at creation and
//! refuse to operate when it has changed.

use anyhow::Result;
use mangle_analysis::StratifiedProgram;
use mangle_ast::Arena;
use mangle_common::Value;
use mangle_interpreter::{Interpreter, MemStore, Store};
use mangle_ir::Ir;
use ouroboros::self_referencing;

use crate::error::{panic_boundary, set_error_msg};
use crate::schema::Schema;
use crate::{MANGLE_ERR_INVALID_ARG, MANGLE_ERR_PARSE, MANGLE_ERR_UNKNOWN_RELATION, MANGLE_OK};

/// Materialized snapshot of every relation in a store: a vec of
/// `(relation_name, tuples)` pairs. Shaped to plug directly into
/// `mangle_db::simplerow::write_simple_row`.
pub(crate) type RelationTables = Vec<(String, Vec<Vec<Value>>)>;

/// Pair holding `Ir` and `StratifiedProgram` together so ouroboros can
/// carry them in a single self-referencing field. `compile_units`
/// returns both at once; splitting them into two separate ouroboros
/// fields isn't possible because the second borrows from the first.
struct CompiledIr<'a> {
    ir: Ir,
    stratified: StratifiedProgram<'a>,
}

/// The compiled + executing program. Owns an `Arena`, an
/// `Ir`/`StratifiedProgram` borrowing from the arena, and an
/// `Interpreter` borrowing from the compiled IR. The three layers are
/// woven together by ouroboros so the engine can be `Box`-allocated and
/// handed out as an opaque pointer.
#[self_referencing]
pub(crate) struct ProgramInner {
    arena: Arena,
    #[borrows(arena)]
    #[not_covariant]
    compiled: CompiledIr<'this>,
    #[borrows(mut compiled)]
    #[not_covariant]
    interp: Interpreter<'this>,
}

/// Opaque engine handle.
///
/// The flag and counter fields are `pub(crate)` so the
/// `panic_boundary!` macro can read/mutate them directly; consumers see
/// only an opaque pointer through the C ABI.
pub struct MangleEngine {
    pub(crate) enable_provenance: bool,
    pub(crate) poisoned: bool,
    pub(crate) generation: u64,
    pub(crate) inner: Option<ProgramInner>,
    /// Cached schema, built at `load_rules` time. `None` when Fresh,
    /// `Some` when Loaded. Touched by every entry point that names a
    /// relation; the schema check rejects unknown-relation accesses
    /// before they reach the store. See `crate::schema` for details.
    pub(crate) schema: Option<Schema>,
}

impl MangleEngine {
    fn new(enable_provenance: bool) -> Self {
        Self {
            enable_provenance,
            poisoned: false,
            generation: 0,
            inner: None,
            schema: None,
        }
    }

    /// Materialize every tuple in `relation` into an owned `Vec<Vec<Value>>`.
    ///
    /// Returns `Ok(None)` when the engine has no rules loaded; returns
    /// `Err` when the relation doesn't exist or scanning fails. The
    /// returned vector is independent of the engine state, so cursors
    /// built on top of it survive engine reload and engine free.
    pub(crate) fn materialize_relation(&self, relation: &str) -> Result<Option<Vec<Vec<Value>>>> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(None);
        };
        inner.with_interp(|interp: &mangle_interpreter::Interpreter<'_>| {
            let store = interp.store();
            let scan = store.scan(relation)?;
            Ok(Some(scan.collect()))
        })
    }

    /// Borrow the engine's schema cache. `None` when Fresh.
    pub(crate) fn schema(&self) -> Option<&Schema> {
        self.schema.as_ref()
    }

    /// Insert a tuple as an EDB fact. Returns `Ok(None)` when no rules
    /// are loaded, else `Ok(Some(added))` where `added` is true iff the
    /// tuple was new to the relation.
    ///
    /// Mirrors `mangle-py`'s insert path: after `Store::insert` places
    /// the tuple in `next_delta`, we call `merge_deltas` twice to
    /// promote it to the stable scan-visible set. IDB relations are
    /// **not** re-derived — same gotcha as mangle-py.
    pub(crate) fn insert_fact(
        &mut self,
        relation: &str,
        tuple: Vec<Value>,
    ) -> Result<Option<bool>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        inner.with_interp_mut(|interp: &mut Interpreter<'_>| {
            let store = interp.store_mut();
            let added = store.insert(relation, tuple)?;
            store.merge_deltas();
            store.merge_deltas();
            Ok(Some(added))
        })
    }

    /// Materialize every relation in the engine's store into a
    /// `Vec<(name, tuples)>` suitable for handoff to
    /// `mangle_db::simplerow::write_simple_row`. Returns `Ok(None)`
    /// when no rules are loaded.
    ///
    /// Relations are emitted in the iteration order of
    /// `Store::relation_names()`. The caller may reorder if a stable
    /// byte output is required.
    pub(crate) fn all_relations_materialized(&self) -> Result<Option<RelationTables>> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(None);
        };
        inner.with_interp(|interp: &Interpreter<'_>| {
            let store = interp.store();
            let names = store.relation_names();
            let mut tables: RelationTables = Vec::with_capacity(names.len());
            for name in names {
                let scan = store.scan(&name)?;
                let tuples: Vec<Vec<Value>> = scan.collect();
                tables.push((name, tuples));
            }
            Ok(Some(tables))
        })
    }

    /// Bulk-insert tuples from a parsed SimpleRow file into the
    /// store. The caller is responsible for parsing + decompression
    /// (in `crate::io`); this method just does the
    /// `store.insert` + double `merge_deltas` dance. Returns
    /// `Ok(None)` when no rules are loaded, else `Ok(Some(n))` where
    /// `n` is the count of tuples passed to `Store::insert` (including
    /// duplicates, which the store silently collapses).
    pub(crate) fn bulk_insert_tables(
        &mut self,
        tables: std::collections::HashMap<String, Vec<Vec<Value>>>,
    ) -> Result<Option<usize>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        inner.with_interp_mut(|interp: &mut Interpreter<'_>| {
            let store = interp.store_mut();
            let mut total: usize = 0;
            for (relation, tuples) in tables {
                for tuple in tuples {
                    store.insert(&relation, tuple)?;
                    total += 1;
                }
            }
            store.merge_deltas();
            store.merge_deltas();
            Ok(Some(total))
        })
    }

    /// Retract a tuple from a relation. Returns `Ok(None)` when no
    /// rules are loaded, else `Ok(Some(found))` where `found` is true
    /// iff the tuple was present and removed. Operates on the stable
    /// set directly; no merge_deltas dance is needed.
    pub(crate) fn retract_fact(&mut self, relation: &str, tuple: &[Value]) -> Result<Option<bool>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        inner.with_interp_mut(|interp: &mut Interpreter<'_>| {
            Ok(Some(interp.store_mut().retract(relation, tuple)?))
        })
    }

    /// Compile + execute the given sources, replacing any existing
    /// loaded program. On failure, the engine's previous state is
    /// preserved (the error path does not clear `inner`). Bumps the
    /// generation on success so any active cursors are invalidated.
    pub(crate) fn load_rules(&mut self, sources: Vec<String>) -> Result<()> {
        let enable_provenance = self.enable_provenance;

        // The schema needs to be built from the just-compiled
        // StratifiedProgram, but ouroboros's `compiled` field has no
        // post-construction accessor (it's mut-borrowed by `interp`).
        // We capture the schema via a side channel inside the
        // `interp_builder` closure, which is the last place we have a
        // borrow of `compiled` during construction. Schema is fully
        // owned (no lifetime tied to the arena), so moving it out
        // afterwards is sound.
        let mut captured_schema: Option<Schema> = None;
        let inner = ProgramInnerTryBuilder {
            arena: Arena::new_with_global_interner(),
            compiled_builder: |arena| -> Result<CompiledIr<'_>> {
                let refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
                let (ir, stratified) = mangle_driver::compile_units(&refs, arena)?;
                Ok(CompiledIr { ir, stratified })
            },
            interp_builder: |compiled: &mut CompiledIr<'_>| -> Result<Interpreter<'_>> {
                captured_schema = Some(Schema::build(&compiled.stratified));
                let store: Box<dyn Store> = Box::new(MemStore::new());
                let CompiledIr { ir, stratified } = compiled;
                let interp = mangle_driver::execute(ir, &*stratified, store)
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(if enable_provenance {
                    interp.with_provenance()
                } else {
                    interp
                })
            },
        }
        .try_build()?;

        self.inner = Some(inner);
        self.schema = captured_schema;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
}

/// Construct a new engine.
///
/// `enable_provenance` is nonzero to record derivation provenance during
/// rule evaluation (M9 surface); zero disables it. The flag is captured
/// here and consulted when rules are loaded.
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

/// Compile and execute one or more Mangle source units, replacing any
/// previously loaded program.
///
/// `sources` is an array of `n_sources` pointers; `lens` parallels it
/// with the byte length of each source. Each source is treated as
/// UTF-8.
///
/// On success, returns [`MANGLE_OK`] and bumps the engine's generation
/// counter (any cursors opened against the previous generation will
/// refuse subsequent reads). On parse / type-check / evaluation
/// failure, the engine's previous state is preserved unchanged and a
/// nonzero status is returned with the formatted error available via
/// [`mangle_last_error`].
///
/// # Safety
/// `engine` must be a live handle from [`mangle_engine_new`].
/// `sources` and `lens` must point to readable arrays of `n_sources`
/// elements; each `sources[i]` must point to `lens[i]` readable bytes
/// of UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_load_rules(
    engine: *mut MangleEngine,
    sources: *const *const u8,
    lens: *const usize,
    n_sources: usize,
) -> i32 {
    panic_boundary!(engine, {
        if n_sources == 0 {
            set_error_msg("mangle_load_rules: at least one source is required");
            return MANGLE_ERR_INVALID_ARG;
        }
        if sources.is_null() || lens.is_null() {
            set_error_msg("mangle_load_rules: sources/lens pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }

        // Materialize each source as an owned String for ownership inside
        // the ouroboros builder. Validates UTF-8 along the way.
        let mut owned: Vec<String> = Vec::with_capacity(n_sources);
        for i in 0..n_sources {
            // SAFETY: caller guarantees the arrays have n_sources entries.
            let ptr = unsafe { *sources.add(i) };
            let len = unsafe { *lens.add(i) };
            if ptr.is_null() && len != 0 {
                set_error_msg(format!(
                    "mangle_load_rules: sources[{i}] is null but length is {len}"
                ));
                return MANGLE_ERR_INVALID_ARG;
            }
            // SAFETY: caller guarantees ptr is valid for `len` bytes.
            let slice = if len == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(ptr, len) }
            };
            match std::str::from_utf8(slice) {
                Ok(s) => owned.push(s.to_string()),
                Err(e) => {
                    set_error_msg(format!(
                        "mangle_load_rules: sources[{i}] is not valid UTF-8: {e}"
                    ));
                    return MANGLE_ERR_INVALID_ARG;
                }
            }
        }

        // SAFETY: engine pointer was non-null and non-poisoned per the
        // panic_boundary pre-checks; the macro doesn't hand the pointer
        // into the closure, so we deref it explicitly here.
        let eng = unsafe { &mut *engine };
        match eng.load_rules(owned) {
            Ok(()) => MANGLE_OK,
            Err(err) => {
                set_error_msg(format!("{err:#}"));
                MANGLE_ERR_PARSE
            }
        }
    })
}

/// Insert a tuple as a fact in the named relation.
///
/// `tuple` is an array of `arity` borrowed `MangleVal` handles. The
/// values are cloned into the engine's store; the source handles
/// remain owned by their producer (typically a [`MangleValBuilder`]).
///
/// On success, `*added_out` (if non-null) is set to 1 when the tuple
/// was new and 0 when the relation already contained it. The store's
/// scan-visible set is updated atomically: subsequent `mangle_query`
/// calls see the new tuple.
///
/// **IDB relations are not re-derived.** Inserting a tuple into a
/// relation that is the head of one or more rules does not cause
/// those rules to fire on the new fact. To re-evaluate the IDB,
/// reload the rules with [`mangle_load_rules`]. This mirrors mangle-py
/// (`PyProgram::insert`).
///
/// Returns [`MANGLE_OK`] on success, [`MANGLE_ERR_NO_RULES`] when no
/// program is loaded, [`MANGLE_ERR_INVALID_ARG`] for null/invalid
/// inputs, or [`MANGLE_ERR`] for a store-level failure.
///
/// # Safety
/// `engine` must be a live handle. `relation` must point to
/// `relation_len` readable UTF-8 bytes. `tuple` must point to `arity`
/// readable `*const MangleVal` entries; each entry must be a live
/// handle. `added_out` is nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_insert_fact(
    engine: *mut MangleEngine,
    relation: *const u8,
    relation_len: usize,
    tuple: *const *const crate::value::MangleVal,
    arity: usize,
    added_out: *mut i32,
) -> i32 {
    panic_boundary!(engine, {
        let relation_str = match read_utf8(relation, relation_len, "mangle_insert_fact: relation") {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let owned = match collect_tuple(tuple, arity, "mangle_insert_fact") {
            Ok(t) => t,
            Err(rc) => return rc,
        };
        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &mut *engine };
        // Schema check (M8): strict mode — inserting into an
        // undeclared relation is a typo, catch it at the entry.
        match eng.schema() {
            Some(s) if !s.knows(&relation_str) => {
                set_error_msg(format!(
                    "mangle_insert_fact: unknown relation `{relation_str}`"
                ));
                return MANGLE_ERR_UNKNOWN_RELATION;
            }
            _ => {}
        }
        match eng.insert_fact(&relation_str, owned) {
            Ok(Some(added)) => {
                if !added_out.is_null() {
                    // SAFETY: caller's contract.
                    unsafe { *added_out = i32::from(added) };
                }
                MANGLE_OK
            }
            Ok(None) => {
                set_error_msg("mangle_insert_fact: engine has no rules loaded");
                crate::MANGLE_ERR_NO_RULES
            }
            Err(e) => {
                set_error_msg(format!("mangle_insert_fact: {e:#}"));
                crate::MANGLE_ERR
            }
        }
    })
}

/// Retract a tuple from the named relation.
///
/// `*found_out` (if non-null) is set to 1 if the tuple was present and
/// removed, 0 otherwise. Operates on the stable set; no merge_deltas
/// is needed.
///
/// Like insert, IDB relations are not automatically reconciled — a
/// retracted EDB fact whose absence would invalidate derived facts
/// leaves stale IDB tuples in place until rules are reloaded.
///
/// # Safety
/// Same as [`mangle_insert_fact`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_retract_fact(
    engine: *mut MangleEngine,
    relation: *const u8,
    relation_len: usize,
    tuple: *const *const crate::value::MangleVal,
    arity: usize,
    found_out: *mut i32,
) -> i32 {
    panic_boundary!(engine, {
        let relation_str = match read_utf8(relation, relation_len, "mangle_retract_fact: relation")
        {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let owned = match collect_tuple(tuple, arity, "mangle_retract_fact") {
            Ok(t) => t,
            Err(rc) => return rc,
        };
        let eng = unsafe { &mut *engine };
        // Schema check (M8): same strict semantics as insert.
        match eng.schema() {
            Some(s) if !s.knows(&relation_str) => {
                set_error_msg(format!(
                    "mangle_retract_fact: unknown relation `{relation_str}`"
                ));
                return MANGLE_ERR_UNKNOWN_RELATION;
            }
            _ => {}
        }
        match eng.retract_fact(&relation_str, &owned) {
            Ok(Some(found)) => {
                if !found_out.is_null() {
                    unsafe { *found_out = i32::from(found) };
                }
                MANGLE_OK
            }
            Ok(None) => {
                set_error_msg("mangle_retract_fact: engine has no rules loaded");
                crate::MANGLE_ERR_NO_RULES
            }
            Err(e) => {
                set_error_msg(format!("mangle_retract_fact: {e:#}"));
                crate::MANGLE_ERR
            }
        }
    })
}

/// Helper: read `len` bytes from `ptr` and decode as UTF-8.
fn read_utf8(ptr: *const u8, len: usize, who: &str) -> std::result::Result<String, i32> {
    if len == 0 {
        return Ok(String::new());
    }
    if ptr.is_null() {
        set_error_msg(format!("{who} pointer is null but length is {len}"));
        return Err(MANGLE_ERR_INVALID_ARG);
    }
    // SAFETY: caller's contract.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    match std::str::from_utf8(slice) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            set_error_msg(format!("{who} is not valid UTF-8: {e}"));
            Err(MANGLE_ERR_INVALID_ARG)
        }
    }
}

/// Helper: read `arity` handles from `tuple` and clone the underlying
/// values into an owned `Vec<Value>`.
fn collect_tuple(
    tuple: *const *const crate::value::MangleVal,
    arity: usize,
    who: &str,
) -> std::result::Result<Vec<Value>, i32> {
    if arity == 0 {
        return Ok(Vec::new());
    }
    if tuple.is_null() {
        set_error_msg(format!("{who}: tuple pointer is null but arity is {arity}"));
        return Err(MANGLE_ERR_INVALID_ARG);
    }
    let mut owned: Vec<Value> = Vec::with_capacity(arity);
    for i in 0..arity {
        // SAFETY: caller guarantees the array has `arity` entries.
        let p = unsafe { *tuple.add(i) };
        if p.is_null() {
            set_error_msg(format!("{who}: tuple[{i}] is null"));
            return Err(MANGLE_ERR_INVALID_ARG);
        }
        // SAFETY: caller guarantees each entry is a live handle.
        owned.push(unsafe { (*p).clone() });
    }
    Ok(owned)
}

/// Internal test helper: deliberately panic inside the engine-bound
/// panic boundary so tests can exercise the poisoning path. Not
/// exported via cbindgen.
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

/// Internal accessor for tests: report the engine's current generation
/// counter. Not exported via cbindgen.
///
/// # Safety
/// `engine` must be null or a live handle from [`mangle_engine_new`].
#[doc(hidden)]
pub unsafe fn engine_generation(engine: *const MangleEngine) -> u64 {
    if engine.is_null() {
        return 0;
    }
    unsafe { (*engine).generation }
}

/// Internal accessor for tests: report whether the engine has rules
/// loaded (`Loaded` state). Not exported via cbindgen.
///
/// # Safety
/// `engine` must be null or a live handle from [`mangle_engine_new`].
#[doc(hidden)]
pub unsafe fn engine_has_rules(engine: *const MangleEngine) -> bool {
    if engine.is_null() {
        return false;
    }
    unsafe { (*engine).inner.is_some() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::take_error;
    use crate::{MANGLE_ERR_INVALID_ARG, MANGLE_ERR_PANIC, MANGLE_ERR_PARSE};
    use std::ptr;

    fn drain_err() {
        let _ = take_error();
    }

    #[test]
    fn new_writes_nonnull_pointer_and_free_releases() {
        let mut p: *mut MangleEngine = ptr::null_mut();
        let rc = unsafe { mangle_engine_new(0, &mut p) };
        assert_eq!(rc, MANGLE_OK);
        assert!(!p.is_null());
        assert_eq!(unsafe { engine_generation(p) }, 0);
        assert!(!unsafe { engine_has_rules(p) });
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn new_records_provenance_flag() {
        let mut p: *mut MangleEngine = ptr::null_mut();
        let _ = unsafe { mangle_engine_new(1, &mut p) };
        assert!(unsafe { (*p).enable_provenance });
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn new_with_null_out_returns_invalid_arg() {
        let rc = unsafe { mangle_engine_new(0, ptr::null_mut()) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
        drain_err();
    }

    #[test]
    fn free_on_null_is_noop() {
        unsafe { mangle_engine_free(ptr::null_mut()) };
    }

    #[test]
    fn engine_bound_panic_marks_poisoned() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };
        let rc = unsafe { force_panic_with_engine(p) };
        assert_eq!(rc, MANGLE_ERR_PANIC);
        assert!(unsafe { (*p).poisoned });
        drain_err();
        unsafe { mangle_engine_free(p) };
    }

    // ----- M2: rule loading -----

    const REACHABLE_MG: &str = "\
        edge(1, 2).\n\
        edge(2, 3).\n\
        edge(3, 4).\n\
        reachable(X, Y) :- edge(X, Y).\n\
        reachable(X, Z) :- edge(X, Y), reachable(Y, Z).\n";

    fn load_one(engine: *mut MangleEngine, src: &str) -> i32 {
        let bytes = src.as_bytes();
        let ptr = bytes.as_ptr();
        let len = bytes.len();
        unsafe { mangle_load_rules(engine, &ptr, &len, 1) }
    }

    #[test]
    fn load_rules_succeeds_and_marks_engine_loaded() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };

        let rc = load_one(p, REACHABLE_MG);
        assert_eq!(rc, MANGLE_OK, "err: {:?}", take_error());
        assert!(unsafe { engine_has_rules(p) });
        assert_eq!(unsafe { engine_generation(p) }, 1);

        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn load_rules_reload_bumps_generation() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };

        assert_eq!(load_one(p, REACHABLE_MG), MANGLE_OK);
        assert_eq!(unsafe { engine_generation(p) }, 1);

        assert_eq!(load_one(p, REACHABLE_MG), MANGLE_OK);
        assert_eq!(unsafe { engine_generation(p) }, 2);

        assert_eq!(load_one(p, REACHABLE_MG), MANGLE_OK);
        assert_eq!(unsafe { engine_generation(p) }, 3);

        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn load_rules_parse_error_preserves_state() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };

        // First load: valid.
        assert_eq!(load_one(p, REACHABLE_MG), MANGLE_OK);
        assert_eq!(unsafe { engine_generation(p) }, 1);
        assert!(unsafe { engine_has_rules(p) });

        // Second load: nonsense.
        let rc = load_one(p, "this is not @@@ mangle .");
        assert_eq!(rc, MANGLE_ERR_PARSE);
        let err = take_error().expect("err set");
        // The error message should mention something parser-shaped.
        assert!(
            err.contains("parse")
                || err.contains("expected")
                || err.contains("unexpected")
                || err.contains("syntax"),
            "expected parse-shaped error, got: {err}"
        );

        // State preserved: still on generation 1, still loaded.
        assert_eq!(unsafe { engine_generation(p) }, 1);
        assert!(unsafe { engine_has_rules(p) });

        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn load_rules_null_engine_returns_invalid_arg() {
        drain_err();
        let src = "edge(1, 2).";
        let ptr = src.as_bytes().as_ptr();
        let len = src.len();
        let rc = unsafe { mangle_load_rules(ptr::null_mut(), &ptr, &len, 1) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
        drain_err();
    }

    #[test]
    fn load_rules_zero_sources_returns_invalid_arg() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };
        let rc = unsafe { mangle_load_rules(p, ptr::null(), ptr::null(), 0) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
        drain_err();
        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn load_rules_invalid_utf8_returns_invalid_arg() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };

        let bad: [u8; 4] = [0xff, 0xfe, 0xfd, 0xfc];
        let ptr = bad.as_ptr();
        let len = bad.len();
        let rc = unsafe { mangle_load_rules(p, &ptr, &len, 1) };
        assert_eq!(rc, MANGLE_ERR_INVALID_ARG);
        let err = take_error().expect("err set");
        assert!(err.contains("UTF-8"), "got: {err}");

        unsafe { mangle_engine_free(p) };
    }

    #[test]
    fn load_rules_on_poisoned_engine_short_circuits() {
        drain_err();
        let mut p: *mut MangleEngine = ptr::null_mut();
        unsafe { mangle_engine_new(0, &mut p) };

        // Poison it.
        unsafe { force_panic_with_engine(p) };
        drain_err();

        // load_rules should refuse.
        let rc = load_one(p, REACHABLE_MG);
        assert_eq!(rc, MANGLE_ERR_PANIC);
        assert!(!unsafe { engine_has_rules(p) });
        let err = take_error().expect("err set");
        assert!(err.contains("poisoned"), "got: {err}");

        unsafe { mangle_engine_free(p) };
    }
}
