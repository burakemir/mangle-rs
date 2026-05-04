use mangle_common::{CompoundKind, Value};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};

/// Lightweight wrapper around a Mangle name constant (e.g. `/role/admin`).
/// Distinct from `str` so round-trips preserve the Name vs. String distinction.
#[pyclass(module = "mangle", name = "Name", frozen, eq, hash)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyName {
    #[pyo3(get)]
    pub value: String,
}

#[pymethods]
impl PyName {
    #[new]
    fn new(value: String) -> PyResult<Self> {
        if !value.starts_with('/') {
            return Err(PyValueError::new_err(
                "Mangle name constants must start with '/'",
            ));
        }
        Ok(Self { value })
    }

    fn __repr__(&self) -> String {
        format!("Name({:?})", self.value)
    }

    fn __str__(&self) -> String {
        self.value.clone()
    }
}

/// Convert a Mangle `Value` into a Python object.
pub fn value_to_pyobj(py: Python<'_>, val: &Value) -> PyResult<PyObject> {
    match val {
        Value::Number(n) => Ok(n.into_py(py)),
        Value::Float(f) => Ok(f.into_py(py)),
        Value::String(s) => Ok(s.clone().into_py(py)),
        Value::Name(n) => {
            let name = PyName { value: n.clone() };
            Ok(Py::new(py, name)?.into_py(py))
        }
        Value::Time(nanos) => time_to_datetime(py, *nanos),
        Value::Duration(nanos) => duration_to_timedelta(py, *nanos),
        Value::Compound(kind, elems) => match kind {
            CompoundKind::List | CompoundKind::Pair => {
                let items: Vec<PyObject> = elems
                    .iter()
                    .map(|v| value_to_pyobj(py, v))
                    .collect::<PyResult<_>>()?;
                Ok(PyList::new_bound(py, items).into_py(py))
            }
            CompoundKind::Map | CompoundKind::Struct => {
                let dict = PyDict::new_bound(py);
                if elems.len() % 2 != 0 {
                    return Err(PyValueError::new_err(
                        "compound map/struct has odd number of elements",
                    ));
                }
                for pair in elems.chunks_exact(2) {
                    let k = value_to_pyobj(py, &pair[0])?;
                    let v = value_to_pyobj(py, &pair[1])?;
                    dict.set_item(k, v)?;
                }
                Ok(dict.into_py(py))
            }
        },
        Value::Null => Ok(py.None()),
    }
}

/// Convert a Python object into a Mangle `Value`.
pub fn pyobj_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    // Order matters: bool is a subclass of int in Python.
    if obj.is_instance_of::<PyBool>() {
        let b: bool = obj.extract()?;
        return Ok(Value::Number(if b { 1 } else { 0 }));
    }
    if let Ok(name) = obj.downcast::<PyName>() {
        return Ok(Value::Name(name.borrow().value.clone()));
    }
    if obj.is_instance_of::<PyInt>() {
        return Ok(Value::Number(obj.extract::<i64>()?));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(obj.extract::<f64>()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::String(obj.extract::<String>()?));
    }
    // datetime / timedelta — duck-typed via attribute checks to avoid linking
    // against the C datetime module.
    if let Some(v) = try_timedelta(obj)? {
        return Ok(v);
    }
    if let Some(v) = try_datetime(obj)? {
        return Ok(v);
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut elems: Vec<Value> = Vec::with_capacity(dict.len() * 2);
        for (k, v) in dict.iter() {
            elems.push(pyobj_to_value(&k)?);
            elems.push(pyobj_to_value(&v)?);
        }
        return Ok(Value::Compound(CompoundKind::Struct, elems));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut elems: Vec<Value> = Vec::with_capacity(list.len());
        for item in list.iter() {
            elems.push(pyobj_to_value(&item)?);
        }
        return Ok(Value::Compound(CompoundKind::List, elems));
    }
    // Treat any sequence (e.g. tuple) as a list.
    if let Ok(seq) = obj.iter() {
        let mut elems: Vec<Value> = Vec::new();
        for item in seq {
            elems.push(pyobj_to_value(&item?)?);
        }
        return Ok(Value::Compound(CompoundKind::List, elems));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert Python object of type {} to mangle Value",
        obj.get_type().name()?,
    )))
}

/// Convert a Python iterable into a Vec<Value>.
pub fn pyobj_to_tuple(obj: &Bound<'_, PyAny>) -> PyResult<Vec<Value>> {
    let iter = obj
        .iter()
        .map_err(|_| PyTypeError::new_err("expected an iterable for a tuple of values"))?;
    let mut out = Vec::new();
    for item in iter {
        out.push(pyobj_to_value(&item?)?);
    }
    Ok(out)
}

fn time_to_datetime(py: Python<'_>, nanos: i64) -> PyResult<PyObject> {
    let datetime = py.import_bound("datetime")?;
    let dt_cls = datetime.getattr("datetime")?;
    let tz = datetime.getattr("timezone")?.getattr("utc")?;
    let secs = nanos.div_euclid(1_000_000_000);
    let ns = nanos.rem_euclid(1_000_000_000) as i64;
    let micros = ns / 1_000;
    let epoch = dt_cls.call_method1("fromtimestamp", (0i64, &tz))?;
    let timedelta = datetime.getattr("timedelta")?;
    let delta = timedelta.call1((0i64, secs, micros))?;
    let dt = epoch.call_method1("__add__", (delta,))?;
    Ok(dt.into_py(py))
}

fn duration_to_timedelta(py: Python<'_>, nanos: i64) -> PyResult<PyObject> {
    let datetime = py.import_bound("datetime")?;
    let timedelta = datetime.getattr("timedelta")?;
    let secs = nanos.div_euclid(1_000_000_000);
    let ns = nanos.rem_euclid(1_000_000_000) as i64;
    let micros = ns / 1_000;
    Ok(timedelta.call1((0i64, secs, micros))?.into_py(py))
}

fn try_timedelta(obj: &Bound<'_, PyAny>) -> PyResult<Option<Value>> {
    let py = obj.py();
    let datetime = py.import_bound("datetime")?;
    let timedelta_cls = datetime.getattr("timedelta")?;
    if !obj.is_instance(&timedelta_cls)? {
        return Ok(None);
    }
    let total: f64 = obj.call_method0("total_seconds")?.extract()?;
    let nanos = (total * 1_000_000_000.0).round() as i64;
    Ok(Some(Value::Duration(nanos)))
}

fn try_datetime(obj: &Bound<'_, PyAny>) -> PyResult<Option<Value>> {
    let py = obj.py();
    let datetime = py.import_bound("datetime")?;
    let dt_cls = datetime.getattr("datetime")?;
    if !obj.is_instance(&dt_cls)? {
        return Ok(None);
    }
    // If naive, assume UTC.
    let aware = if obj.getattr("tzinfo")?.is_none() {
        let tz_utc = datetime.getattr("timezone")?.getattr("utc")?;
        let kwargs = PyDict::new_bound(py);
        kwargs.set_item("tzinfo", tz_utc)?;
        obj.call_method("replace", (), Some(&kwargs))?
    } else {
        obj.clone()
    };
    let ts: f64 = aware.call_method0("timestamp")?.extract()?;
    let nanos = (ts * 1_000_000_000.0).round() as i64;
    Ok(Some(Value::Time(nanos)))
}
