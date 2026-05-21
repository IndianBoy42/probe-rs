//! Error handling for the pyprobe-rs bridge.

use pyo3::prelude::*;

/// Convert any error implementing Display into a PyErr.
pub fn to_py_err<E: std::fmt::Display>(e: E) -> PyErr {
    pyo3::exceptions::PyException::new_err(format!("{e}"))
}
