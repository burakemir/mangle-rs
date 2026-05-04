use pyo3::prelude::*;

mod error;
mod program;
mod query;
mod value;

use error::MangleError;
use program::{PyProgram, eval_source_py, eval_units_py};
use value::PyName;

/// One-shot: compile and execute, optionally filtering by a query atom.
#[pyfunction]
#[pyo3(signature = (source, query=None))]
fn eval(py: Python<'_>, source: &str, query: Option<&str>) -> PyResult<PyObject> {
    eval_source_py(py, source, query)
}

/// Like [`eval`] but takes multiple source units (Package/Use across files).
#[pyfunction]
#[pyo3(signature = (sources, query=None))]
fn eval_units(py: Python<'_>, sources: Vec<String>, query: Option<&str>) -> PyResult<PyObject> {
    eval_units_py(py, sources, query)
}

#[pymodule]
fn mangle(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("MangleError", py.get_type_bound::<MangleError>())?;
    m.add_class::<PyProgram>()?;
    m.add_class::<PyName>()?;
    m.add_function(wrap_pyfunction!(eval, m)?)?;
    m.add_function(wrap_pyfunction!(eval_units, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
