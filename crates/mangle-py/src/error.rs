use pyo3::create_exception;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

create_exception!(mangle, MangleError, PyRuntimeError);

pub fn anyhow_to_pyerr(err: anyhow::Error) -> PyErr {
    MangleError::new_err(format!("{:#}", err))
}

pub trait IntoPyResult<T> {
    fn into_py(self) -> PyResult<T>;
}

impl<T> IntoPyResult<T> for anyhow::Result<T> {
    fn into_py(self) -> PyResult<T> {
        self.map_err(anyhow_to_pyerr)
    }
}
