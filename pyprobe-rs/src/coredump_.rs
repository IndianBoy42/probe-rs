//! CoreDump support.
//!
//! The dump is taken server-side via the `core/dump` endpoint and
//! reconstructed into a local `probe_rs::CoreDump` (every field is public,
//! so the wire round-trip loses nothing). `store`/`load` then work exactly
//! like the direct-probe implementation, including the ELFcoredump format.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use probe_rs::CoreDump as RsCoreDump;
use pyo3::prelude::*;

use crate::core_::{
    wire_core_type_to_local, wire_instruction_set_to_local, wire_register_value_to_local,
};
use crate::error::{to_bridge_err, to_py_err};
use crate::rpc::{Connection, call_with_recovery, rpc_call};
use crate::session::Session;

#[pyclass]
pub struct CoreDump {
    inner: Option<RsCoreDump>,
}

/// Core dumps read every requested RAM range in one RPC — minutes-class on
/// slow probes, so this gets the flash-class budget instead of the default.
const DUMP_TIMEOUT: Duration = Duration::from_secs(600);

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
        py: Python<'_>,
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
        let conn: Arc<Connection> = session.conn()?;
        let wire = py
            .detach(|| {
                call_with_recovery(&conn, DUMP_TIMEOUT, "core dump", || {
                    rpc_call(
                        DUMP_TIMEOUT,
                        "core dump",
                        conn.session()?.core(core_index).dump_core(ranges.clone()),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        Ok(Self {
            inner: Some(reconstruct(wire)),
        })
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

/// Rebuild a local `probe_rs::CoreDump` from the wire fields.
fn reconstruct(wire: probe_rs_rpc::core_ops::WireCoreDump) -> RsCoreDump {
    RsCoreDump {
        registers: wire
            .registers
            .into_iter()
            .map(|(id, v)| (probe_rs::RegisterId(id.0), wire_register_value_to_local(v)))
            .collect(),
        data: wire.data,
        instruction_set: wire_instruction_set_to_local(wire.instruction_set),
        supports_native_64bit_access: wire.supports_native_64bit_access,
        core_type: wire_core_type_to_local(wire.core_type),
        fpu_support: wire.fpu_support,
        floating_point_register_count: wire.floating_point_register_count.map(|c| c as usize),
    }
}
