use anyhow::{Result, anyhow};
use mangle_analysis::StratifiedProgram;
use mangle_ast::Arena;
use mangle_common::Value;
use mangle_interpreter::{Interpreter, MemStore};
use mangle_ir::Ir;
use ouroboros::self_referencing;
use pyo3::prelude::*;
use pyo3::types::PyList;

use crate::error::IntoPyResult;
use crate::query::{filter_tuples, parse_query_lenient};
use crate::value::{pyobj_to_tuple, value_to_pyobj};

/// Bundles `Ir` and `StratifiedProgram` so ouroboros can carry both in a
/// single self-referenced field. (`compile_units` returns them together; we
/// can't split into two separate ouroboros fields.)
struct CompiledIr<'a> {
    ir: Ir,
    stratified: StratifiedProgram<'a>,
}

#[self_referencing]
struct ProgramInner {
    arena: Arena,
    #[borrows(arena)]
    #[not_covariant]
    compiled: CompiledIr<'this>,
    #[borrows(mut compiled)]
    #[not_covariant]
    interp: Interpreter<'this>,
}

fn compile_in_arena<'a>(
    arena: &'a Arena,
    sources: &[&str],
) -> Result<CompiledIr<'a>> {
    let (ir, stratified) = mangle_driver::compile_units(sources, arena)?;
    Ok(CompiledIr { ir, stratified })
}

fn build_inner(sources: Vec<String>) -> Result<ProgramInner> {
    let inner = ProgramInnerTryBuilder {
        arena: Arena::new_with_global_interner(),
        compiled_builder: |arena| -> Result<CompiledIr<'_>> {
            let refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
            compile_in_arena(arena, &refs)
        },
        interp_builder: |compiled: &mut CompiledIr<'_>| -> Result<Interpreter<'_>> {
            let store: Box<dyn mangle_interpreter::Store> = Box::new(MemStore::new());
            let CompiledIr { ir, stratified } = compiled;
            mangle_driver::execute(ir, &*stratified, store)
                .map_err(|e| anyhow!(e))
        },
    }
    .try_build()?;
    Ok(inner)
}

/// A compiled and evaluated Mangle program.
///
/// Construction compiles the source(s) and runs the interpreter once. The
/// resulting facts are queryable via `query()` and `relations()`. EDB facts
/// can be added/removed via `insert()` and `retract()`, but rules are not
/// re-derived automatically; create a new `Program` to re-evaluate.
#[pyclass(module = "mangle", name = "Program", unsendable)]
pub struct PyProgram {
    inner: ProgramInner,
}

#[pymethods]
impl PyProgram {
    #[new]
    fn new(source: &str) -> PyResult<Self> {
        let inner = build_inner(vec![source.to_string()]).into_py()?;
        Ok(Self { inner })
    }

    /// Compile and execute multiple source units (with Package/Use directives).
    #[staticmethod]
    fn from_units(sources: Vec<String>) -> PyResult<Self> {
        if sources.is_empty() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "from_units requires at least one source",
            ));
        }
        let inner = build_inner(sources).into_py()?;
        Ok(Self { inner })
    }

    /// Query a relation, returning all matching tuples.
    ///
    /// `query` may be a bare predicate name (e.g. `"q"`) or a query atom with
    /// constants used as filters (e.g. `r#"route("GET", Path)"#`). Variables
    /// (uppercase identifiers) are wildcards.
    fn query(&self, py: Python<'_>, query: &str) -> PyResult<PyObject> {
        let parsed = parse_query_lenient(query).into_py()?;
        let tuples: Vec<Vec<Value>> = self.inner.with_interp(|interp| {
            interp
                .store()
                .scan(&parsed.predicate)
                .map(|it| it.collect::<Vec<_>>())
        }).into_py()?;
        let filtered = filter_tuples(tuples, &parsed);
        tuples_to_pylist(py, &filtered)
    }

    /// List all relation names known to the program.
    fn relations(&self) -> Vec<String> {
        self.inner
            .with_interp(|interp| interp.store().relation_names())
    }

    /// Insert a tuple as an EDB fact and promote it into the stable fact set.
    ///
    /// Note: rules are not re-evaluated. Subsequent `query()` calls will see
    /// this fact in the named relation, but downstream IDB relations will not
    /// reflect it. Create a new `Program` to re-derive.
    fn insert(&mut self, relation: &str, tuple: Bound<'_, PyAny>) -> PyResult<bool> {
        let row = pyobj_to_tuple(&tuple)?;
        self.inner
            .with_interp_mut(|interp| -> anyhow::Result<bool> {
                let store = interp.store_mut();
                let added = store.insert(relation, row)?;
                // insert() places facts in next_delta. Promote to stable so
                // subsequent scan() calls see them.
                store.merge_deltas();
                store.merge_deltas();
                Ok(added)
            })
            .into_py()
    }

    /// Retract a tuple from a relation. Returns True if the tuple was found.
    fn retract(&mut self, relation: &str, tuple: Bound<'_, PyAny>) -> PyResult<bool> {
        let row = pyobj_to_tuple(&tuple)?;
        self.inner
            .with_interp_mut(|interp| interp.store_mut().retract(relation, &row))
            .into_py()
    }
}

fn tuples_to_pylist(py: Python<'_>, tuples: &[Vec<Value>]) -> PyResult<PyObject> {
    let outer = PyList::empty_bound(py);
    for tuple in tuples {
        let row = PyList::empty_bound(py);
        for v in tuple {
            row.append(value_to_pyobj(py, v)?)?;
        }
        outer.append(row)?;
    }
    Ok(outer.into_py(py))
}

/// One-shot evaluation: compile, execute, return tuples.
pub fn eval_source_py(
    py: Python<'_>,
    source: &str,
    query: Option<&str>,
) -> PyResult<PyObject> {
    eval_units_py(py, vec![source.to_string()], query)
}

pub fn eval_units_py(
    py: Python<'_>,
    sources: Vec<String>,
    query: Option<&str>,
) -> PyResult<PyObject> {
    let arena = Arena::new_with_global_interner();
    let refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
    let (mut ir, stratified) = mangle_driver::compile_units(&refs, &arena).into_py()?;
    let store: Box<dyn mangle_interpreter::Store> = Box::new(MemStore::new());
    let interp = mangle_driver::execute(&mut ir, &stratified, store).into_py()?;

    let tuples: Vec<Vec<Value>> = if let Some(q) = query {
        let parsed = parse_query_lenient(q).into_py()?;
        let scanned: Vec<Vec<Value>> = interp
            .store()
            .scan(&parsed.predicate)
            .into_py()?
            .collect();
        filter_tuples(scanned, &parsed)
    } else {
        let mut all = Vec::new();
        for name in interp.store().relation_names() {
            let scanned: Vec<Vec<Value>> = interp.store().scan(&name).into_py()?.collect();
            all.extend(scanned);
        }
        all
    };
    tuples_to_pylist(py, &tuples)
}
