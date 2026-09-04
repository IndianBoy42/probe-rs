//! CoreDump support.

use crate::error::to_py_err;
use crate::session::Session;
use pyo3::prelude::*;
use probe_rs::CoreDump as RsCoreDump;
use std::collections::HashMap;
use std::ops::Range;

#[pyclass(unsendable)]
pub struct CoreDump {
    inner: Option<RsCoreDump>,
}

#[pymethods]
impl CoreDump {
    /// Load a core dump from a file.
    #[staticmethod]
    pub fn load(path: &str) -> PyResult<Self> {
        let data = RsCoreDump::load(std::path::Path::new(path)).map_err(to_py_err)?;
        Ok(Self { inner: Some(data) })
    }

    /// Capture a core dump from a live session core.
    ///
    /// `ranges` are inclusive-exclusive (start, end) memory ranges to record.
    #[staticmethod]
    pub fn capture(
        session: &mut Session,
        core_index: usize,
        ranges: Vec<(u64, u64)>,
    ) -> PyResult<Self> {
        let ranges: Vec<Range<u64>> = ranges
            .into_iter()
            .map(|(start, end)| {
                if end < start {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "invalid memory range: {start:#x}..{end:#x}"
                    )));
                }
                Ok(start..end)
            })
            .collect::<PyResult<Vec<_>>>()?;
        let sess = &mut session.inner;
        let mut core = sess.core(core_index).map_err(to_py_err)?;
        let dump = RsCoreDump::dump_core(&mut core, ranges).map_err(to_py_err)?;
        Ok(Self { inner: Some(dump) })
    }

    /// Save the core dump to a file.
    pub fn store(&self, path: &str) -> PyResult<()> {
        let dump = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("dump consumed"))?;
        dump.store(std::path::Path::new(path)).map_err(to_py_err)
    }

    /// Register values (id → value).
    #[getter]
    pub fn registers(&self) -> PyResult<HashMap<u16, u128>> {
        let dump = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("dump consumed"))?;
        let mut out = HashMap::new();
        for (id, val) in &dump.registers {
            let v: u128 = match val {
                probe_rs::RegisterValue::U32(v) => *v as u128,
                probe_rs::RegisterValue::U64(v) => *v as u128,
                probe_rs::RegisterValue::U128(v) => *v,
            };
            out.insert(id.0, v);
        }
        Ok(out)
    }

    /// Memory segments as (address, bytes) pairs.
    #[getter]
    pub fn data(&self) -> PyResult<Vec<(u64, Vec<u8>)>> {
        let dump = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("dump consumed"))?;
        Ok(dump
            .data
            .iter()
            .map(|(range, bytes)| (range.start, bytes.clone()))
            .collect())
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(_) => "CoreDump".into(),
            None => "CoreDump(consumed)".into(),
        }
    }
}
