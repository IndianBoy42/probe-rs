//! Probe enumeration, opening, and FakeProbe.

use crate::error::to_py_err;
use crate::session::{Permissions, Session};
use probe_rs::probe::{DebugProbeInfo, Probe, WireProtocol, list::Lister};
use pyo3::prelude::*;
use std::str::FromStr;

use crate::types::WireProtocolProxy;

/// A registry of available debug probes on the system.
#[pyclass(unsendable, name = "Lister")]
pub struct ListerPy {
    lister: Lister,
}

#[pymethods]
impl ListerPy {
    #[new]
    pub fn new() -> Self {
        Self {
            lister: Lister::new(),
        }
    }

    /// List all probes detected on the system.
    pub fn list_all(&self) -> Vec<ProbeInfo> {
        self.lister
            .list_all()
            .into_iter()
            .map(ProbeInfo::from)
            .collect()
    }

    /// Open a probe by selector string ("VID:PID[:SN]") or by a
    /// DebugProbeSelector(vid, pid, serial) instance.
    pub fn open(&self, selector: &Bound<'_, PyAny>) -> PyResult<ProbePy> {
        let sel: probe_rs::probe::DebugProbeSelector = if let Ok(s) = selector.extract::<String>() {
            probe_rs::probe::DebugProbeSelector::from_str(&s).map_err(to_py_err)?
        } else if let Ok(p) = selector.extract::<DebugProbeSelectorPy>() {
            probe_rs::probe::DebugProbeSelector {
                vendor_id: p.vid,
                product_id: p.pid,
                interface: None,
                serial_number: p.serial,
            }
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "selector must be a 'VID:PID[:SN]' string or a DebugProbeSelector",
            ));
        };
        self.lister.open(sel).map(ProbePy::from).map_err(to_py_err)
    }
}

/// Information about a detected debug probe.
#[pyclass(unsendable)]
pub struct ProbeInfo {
    inner: DebugProbeInfo,
}

impl From<DebugProbeInfo> for ProbeInfo {
    fn from(info: DebugProbeInfo) -> Self {
        Self { inner: info }
    }
}

#[pymethods]
impl ProbeInfo {
    /// Vendor ID.
    #[getter]
    fn vid(&self) -> u16 {
        self.inner.vendor_id
    }

    /// Product ID.
    #[getter]
    fn pid(&self) -> u16 {
        self.inner.product_id
    }

    /// Serial number (if available).
    #[getter]
    fn serial(&self) -> Option<String> {
        self.inner.serial_number.clone()
    }

    /// Probe identifier name.
    #[getter]
    fn identifier(&self) -> String {
        self.inner.identifier.clone()
    }

    /// Open this probe.
    fn open(&self) -> PyResult<ProbePy> {
        self.inner.open().map(ProbePy::from).map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        format!(
            "ProbeInfo(vid={:#06x}, pid={:#06x}, serial={:?})",
            self.inner.vendor_id, self.inner.product_id, self.inner.serial_number
        )
    }
}

/// An open debug probe.
#[pyclass(unsendable, name = "Probe")]
pub struct ProbePy {
    pub(crate) inner: Option<Probe>,
}

impl From<Probe> for ProbePy {
    fn from(p: Probe) -> Self {
        Self { inner: Some(p) }
    }
}

#[pymethods]
impl ProbePy {
    /// Attach to a target by name.
    #[pyo3(signature = (target, permissions=None))]
    pub fn attach(&mut self, target: &str, permissions: Option<Permissions>) -> PyResult<Session> {
        let perms = permissions.unwrap_or_default().inner;
        let probe = self
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe
            .attach(target, perms)
            .map_err(to_py_err)
            .map(Session::from)
    }

    /// Attach under reset.
    #[pyo3(signature = (target, permissions=None))]
    pub fn attach_under_reset(
        &mut self,
        target: &str,
        permissions: Option<Permissions>,
    ) -> PyResult<Session> {
        let perms = permissions.unwrap_or_default().inner;
        let probe = self
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe
            .attach_under_reset(target, perms)
            .map_err(to_py_err)
            .map(Session::from)
    }

    pub fn set_speed(&mut self, khz: u32) -> PyResult<u32> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.set_speed(khz).map_err(to_py_err)
    }

    pub fn select_protocol(&mut self, protocol: WireProtocolProxy) -> PyResult<()> {
        let wp = match protocol {
            WireProtocolProxy::Swd => WireProtocol::Swd,
            WireProtocolProxy::Jtag => WireProtocol::Jtag,
        };
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.select_protocol(wp).map_err(to_py_err)
    }

    pub fn protocol(&self) -> PyResult<Option<WireProtocolProxy>> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.protocol().map(|p| match p {
            WireProtocol::Swd => WireProtocolProxy::Swd,
            WireProtocol::Jtag => WireProtocolProxy::Jtag,
        }))
    }

    pub fn detach(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.detach().map_err(to_py_err)
    }

    pub fn target_reset(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.target_reset().map_err(to_py_err)
    }

    pub fn target_reset_assert(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.target_reset_assert().map_err(to_py_err)
    }

    pub fn target_reset_deassert(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.target_reset_deassert().map_err(to_py_err)
    }

    pub fn get_target_voltage(&mut self) -> PyResult<Option<f32>> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        probe.get_target_voltage().map_err(to_py_err)
    }

    pub fn get_name(&self) -> PyResult<String> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.get_name())
    }

    pub fn speed_khz(&self) -> PyResult<u32> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.speed_khz())
    }

    pub fn has_arm_debug_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.has_arm_debug_interface())
    }

    pub fn has_riscv_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.has_riscv_interface())
    }

    pub fn has_xtensa_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Probe consumed"))?;
        Ok(probe.has_xtensa_interface())
    }

    /// Construct a Probe from a FakeProbe (consumes the FakeProbe).
    #[staticmethod]
    pub fn from_fake(fp: &mut FakeProbePy) -> PyResult<ProbePy> {
        let inner = fp
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("FakeProbe already consumed"))?;
        Ok(ProbePy::from(inner.into_probe()))
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(p) => format!("Probe(name={})", p.get_name()),
            None => "Probe(consumed)".into(),
        }
    }
}

/// A fake probe for testing without hardware.
#[pyclass(unsendable, name = "FakeProbe")]
pub struct FakeProbePy {
    pub(crate) inner: Option<probe_rs::probe::fake_probe::FakeProbe>,
}

#[pymethods]
impl FakeProbePy {
    /// Create a FakeProbe with a mocked core.
    #[staticmethod]
    pub fn with_mocked_core() -> Self {
        Self {
            inner: Some(probe_rs::probe::fake_probe::FakeProbe::with_mocked_core()),
        }
    }

    /// Create a bare FakeProbe.
    #[staticmethod]
    pub fn new() -> Self {
        Self {
            inner: Some(probe_rs::probe::fake_probe::FakeProbe::new()),
        }
    }

    /// Convert into a real Probe handle.
    pub fn into_probe(&mut self) -> PyResult<ProbePy> {
        let fp = self
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("FakeProbe already consumed"))?;
        Ok(ProbePy::from(fp.into_probe()))
    }
}

/// A debug probe selector (VID:PID[:SN]).
#[pyclass(name = "DebugProbeSelector", from_py_object)]
#[derive(Clone)]
pub struct DebugProbeSelectorPy {
    #[pyo3(get)]
    pub vid: u16,
    #[pyo3(get)]
    pub pid: u16,
    #[pyo3(get)]
    pub serial: Option<String>,
}

#[pymethods]
impl DebugProbeSelectorPy {
    #[new]
    #[pyo3(signature = (vid, pid, serial=None))]
    pub fn new(vid: u16, pid: u16, serial: Option<String>) -> Self {
        Self { vid, pid, serial }
    }

    fn __str__(&self) -> String {
        match &self.serial {
            Some(s) => format!("{:04x}:{:04x}:{}", self.vid, self.pid, s),
            None => format!("{:04x}:{:04x}", self.vid, self.pid),
        }
    }

    fn __repr__(&self) -> String {
        format!("DebugProbeSelector({})", self.__str__())
    }
}
