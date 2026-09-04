//! Session, SessionConfig, Permissions, Registry, Target.

use crate::error::to_py_err;
use crate::types::*;
use probe_rs::config::{Registry as RsRegistry, Target as RsTarget};
use probe_rs::{
    Permissions as RsPermissions, Session as RsSession, SessionConfig as RsSessionConfig,
};
use probe_rs_target::MemoryRegion;
use pyo3::prelude::*;
use std::io::Read;

/// Session configuration.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct SessionConfig {
    #[pyo3(get, set)]
    pub speed: Option<u32>,
    #[pyo3(get, set)]
    pub protocol: Option<WireProtocolProxy>,
    #[pyo3(get, set)]
    pub permissions: Option<Permissions>,
}

impl SessionConfig {
    pub fn into_rs(self) -> RsSessionConfig {
        let mut cfg = RsSessionConfig::default();
        if let Some(s) = self.speed {
            cfg.speed = Some(s);
        }
        if let Some(p) = self.protocol {
            cfg.protocol = Some(match p {
                WireProtocolProxy::Swd => probe_rs::probe::WireProtocol::Swd,
                WireProtocolProxy::Jtag => probe_rs::probe::WireProtocol::Jtag,
            });
        }
        if let Some(perms) = self.permissions {
            cfg.permissions = perms.inner;
        }
        cfg
    }
}

impl From<Option<Permissions>> for SessionConfig {
    fn from(perms: Option<Permissions>) -> Self {
        Self {
            speed: None,
            protocol: None,
            permissions: perms,
        }
    }
}

#[pymethods]
impl SessionConfig {
    #[new]
    #[pyo3(signature = (speed=None, protocol=None, permissions=None))]
    pub fn new(
        speed: Option<u32>,
        protocol: Option<WireProtocolProxy>,
        permissions: Option<Permissions>,
    ) -> Self {
        Self {
            speed,
            protocol,
            permissions,
        }
    }
}

/// Permissions granted to a debug session.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct Permissions {
    pub(crate) inner: RsPermissions,
}

#[pymethods]
impl Permissions {
    #[new]
    pub fn new() -> Self {
        Self {
            inner: RsPermissions::new(),
        }
    }

    /// Allow the erase-all sequence.
    pub fn allow_erase_all(&mut self) {
        self.inner = self.inner.clone().allow_erase_all();
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self::new()
    }
}

/// Target specification (read-only from Python).
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct Target {
    pub(crate) inner: RsTarget,
}

#[pymethods]
impl Target {
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn architecture(&self) -> ArchitectureProxy {
        architecture_to_py(self.inner.architecture())
    }

    #[getter]
    fn cores(&self) -> Vec<(String, CoreTypeProxy)> {
        self.inner
            .cores
            .iter()
            .map(|c| (c.name.clone(), core_type_to_py(c.core_type)))
            .collect()
    }

    #[getter]
    fn flash_algorithms(&self) -> Vec<String> {
        self.inner
            .flash_algorithms
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }

    #[getter]
    fn memory_map(&self) -> Vec<(String, u64, u64)> {
        self.inner
            .memory_map
            .iter()
            .map(|r| match r {
                MemoryRegion::Nvm(r) => ("nvm", r.range.start, r.range.end),
                MemoryRegion::Ram(r) => ("ram", r.range.start, r.range.end),
                MemoryRegion::Generic(r) => ("generic", r.range.start, r.range.end),
            })
            .map(|(k, s, e)| (k.to_string(), s, e))
            .collect()
    }
}

/// Target registry: lookup targets by name/family.
#[pyclass(name = "Registry")]
pub struct RegistryPy {
    inner: RsRegistry,
}

#[pymethods]
impl RegistryPy {
    #[staticmethod]
    fn from_builtin_families() -> Self {
        Self {
            inner: RsRegistry::from_builtin_families(),
        }
    }

    /// Get all chip family names.
    fn families(&self) -> Vec<String> {
        self.inner
            .families()
            .iter()
            .map(|f| f.name.clone())
            .collect()
    }

    /// Get a target by name.
    fn get_target_by_name(&self, name: &str) -> PyResult<Target> {
        self.inner
            .get_target_by_name(name)
            .map(|t| Target { inner: t })
            .map_err(to_py_err)
    }

    /// Search chips by prefix.
    fn search_chips(&self, prefix: &str) -> Vec<String> {
        self.inner
            .search_chips(prefix)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[pyclass(unsendable)]
pub struct Session {
    pub(crate) inner: RsSession,
}

impl From<RsSession> for Session {
    fn from(s: RsSession) -> Self {
        Self { inner: s }
    }
}

impl Session {
    pub(crate) fn get_mut(&mut self) -> &mut RsSession {
        &mut self.inner
    }
}

#[pymethods]
impl Session {
    /// Auto-attach to a target by name.
    #[staticmethod]
    #[pyo3(signature = (target, config=None))]
    pub fn auto_attach(target: &str, config: Option<SessionConfig>) -> PyResult<Self> {
        let cfg = config.map(|c| c.into_rs()).unwrap_or_default();
        RsSession::auto_attach(target, cfg)
            .map_err(to_py_err)
            .map(Session::from)
    }

    /// Get a CoreHandle for the core at `core_index`.
    pub fn core(slf: &Bound<'_, Self>, core_index: usize) -> PyResult<crate::core_::CoreHandle> {
        let borrow = slf.borrow();
        let cores = borrow.inner.list_cores();
        if core_index >= cores.len() {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "core index {core_index} out of range (have {} cores)",
                cores.len()
            )));
        }
        drop(borrow);
        Ok(crate::core_::CoreHandle::new(
            slf.clone().unbind(),
            core_index,
        ))
    }

    /// List available cores as (index, CoreType) tuples.
    pub fn list_cores(&self) -> Vec<(usize, CoreTypeProxy)> {
        self.inner
            .list_cores()
            .into_iter()
            .map(|(i, ct)| (i, core_type_to_py(ct)))
            .collect()
    }

    /// Get the target specification.
    pub fn target(&self) -> PyResult<Target> {
        Ok(Target {
            inner: self.inner.target().clone(),
        })
    }

    /// Get the target architecture.
    pub fn architecture(&self) -> PyResult<ArchitectureProxy> {
        Ok(architecture_to_py(self.inner.architecture()))
    }

    /// Clear all hardware breakpoints across all cores.
    pub fn clear_all_hw_breakpoints(&mut self) -> PyResult<()> {
        self.get_mut().clear_all_hw_breakpoints().map_err(to_py_err)
    }

    /// Resume all cores.
    pub fn resume_all_cores(&mut self) -> PyResult<()> {
        self.get_mut().resume_all_cores().map_err(to_py_err)
    }

    /// Check if the debug sequence supports erase-all.
    pub fn has_sequence_erase_all(&mut self) -> PyResult<bool> {
        Ok(self.get_mut().has_sequence_erase_all())
    }

    /// Run the erase-all sequence.
    pub fn sequence_erase_all(&mut self) -> PyResult<()> {
        self.get_mut().sequence_erase_all().map_err(to_py_err)
    }

    // ── SWO / Trace ────────────────────────────────────────────────

    /// Set up tracing for a core.
    #[pyo3(signature = (core_index, sink))]
    pub fn setup_tracing(&mut self, core_index: usize, sink: TraceSinkProxy) -> PyResult<()> {
        use probe_rs::architecture::arm::component::TraceSink;
        use probe_rs::architecture::arm::swo::SwoConfig;
        let sink = match sink {
            TraceSinkProxy::Swo => TraceSink::Swo(SwoConfig::new(0)),
            TraceSinkProxy::Tpiu => TraceSink::Tpiu(SwoConfig::new(0)),
            TraceSinkProxy::TraceMemory => TraceSink::TraceMemory,
        };
        self.get_mut()
            .setup_tracing(core_index, sink)
            .map_err(to_py_err)
    }

    /// Disable SWV for a core.
    pub fn disable_swv(&mut self, core_index: usize) -> PyResult<()> {
        self.get_mut().disable_swv(core_index).map_err(to_py_err)
    }

    /// Add a SWV data trace unit.
    pub fn add_swv_data_trace(&mut self, unit: usize, addr: u64) -> PyResult<()> {
        self.get_mut()
            .add_swv_data_trace(unit, addr as u32)
            .map_err(to_py_err)
    }

    /// Remove a SWV data trace unit.
    pub fn remove_swv_data_trace(&mut self, unit: usize) -> PyResult<()> {
        self.get_mut()
            .remove_swv_data_trace(unit)
            .map_err(to_py_err)
    }

    /// Read available trace/SWO data.
    pub fn read_trace_data(&mut self) -> PyResult<Vec<u8>> {
        let sess = self.get_mut();
        let mut buf = Vec::new();
        let mut reader = sess.swo_reader().map_err(to_py_err)?;
        reader.read_to_end(&mut buf).map_err(to_py_err)?;
        Ok(buf)
    }

    /// Read available SWO data.
    pub fn read_swo(&mut self) -> PyResult<Vec<u8>> {
        self.read_trace_data()
    }

    /// Erase all flash: the vendor erase-all sequence when the target provides
    /// one, otherwise a full chip erase performed through the flash loader.
    pub fn erase_all(&mut self) -> PyResult<()> {
        let sess = self.get_mut();
        if sess.has_sequence_erase_all() {
            sess.sequence_erase_all().map_err(to_py_err)
        } else {
            let mut progress = probe_rs::flashing::FlashProgress::empty();
            probe_rs::flashing::erase_all(sess, &mut progress, false).map_err(to_py_err)
        }
    }

    // ── RTT ────────────────────────────────────────────────────────

    /// Attach to RTT on the given core, optionally at a specific pointer.
    #[pyo3(signature = (core_index=0, ptr=None))]
    pub fn attach_rtt(
        slf: &Bound<'_, Self>,
        core_index: usize,
        ptr: Option<u64>,
    ) -> PyResult<crate::rtt_::Rtt> {
        crate::rtt_::attach(slf, core_index, ptr)
    }

    fn __repr__(&self) -> String {
        format!("Session(target={})", self.inner.target().name)
    }
}
