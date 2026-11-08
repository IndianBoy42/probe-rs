//! Probe enumeration, opening, and FakeProbe.

use crate::error::{py_err_with_kind, to_py_err};
use crate::session::{Permissions, Session};
use probe_rs::probe::{DebugProbeInfo, Probe, WireProtocol, list::Lister};
use probe_rs_rpc::probe::DebugProbeEntry;
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
                usb_location: None,
            }
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "selector must be a 'VID:PID[:SN]' string or a DebugProbeSelector",
            ));
        };
        let probe = self.lister.open(sel.clone()).map_err(to_py_err)?;
        Ok(ProbePy {
            inner: Some(probe),
            selector: Some(sel),
            is_fake: false,
            speed_khz: None,
            protocol: None,
            enumerated_remotely: false,
        })
    }
}

/// Information about a detected debug probe.
#[pyclass(unsendable)]
pub struct ProbeInfo {
    identifier: String,
    vendor_id: u16,
    product_id: u16,
    interface: Option<u8>,
    serial_number: Option<String>,
    /// USB bus ID and device address. Distinguishes probes that share a
    /// VID/PID/serial; `None` for locally-listed probes whose info omits it.
    usb_location: Option<(String, u8)>,
    probe_type: String,
    inaccessible: bool,
    /// Present only for locally-listed probes; RPC listings carry no probe
    /// factory handle, so only local entries hold one.
    local: Option<DebugProbeInfo>,
}

impl From<DebugProbeInfo> for ProbeInfo {
    fn from(info: DebugProbeInfo) -> Self {
        Self {
            identifier: info.identifier.clone(),
            vendor_id: info.vendor_id,
            product_id: info.product_id,
            // DebugProbeInfo cannot expose its interface (private field).
            interface: None,
            serial_number: info.serial_number.clone(),
            usb_location: info.usb_location.clone(),
            // DebugProbeInfo cannot expose its probe factory (private
            // field), so the type name is only known for RPC listings.
            probe_type: String::new(),
            inaccessible: false,
            local: Some(info),
        }
    }
}

impl ProbeInfo {
    /// Build from an RPC `probe/list` entry (no local open handle).
    pub(crate) fn from_entry(entry: &DebugProbeEntry) -> Self {
        Self {
            identifier: entry.identifier.clone(),
            vendor_id: entry.vendor_id,
            product_id: entry.product_id,
            interface: entry.interface,
            serial_number: Some(entry.serial_number.clone()).filter(|s| !s.is_empty()),
            usb_location: entry.usb_location.clone(),
            probe_type: entry.probe_type.clone(),
            inaccessible: entry.inaccessible,
            local: None,
        }
    }
}

#[pymethods]
impl ProbeInfo {
    /// Vendor ID.
    #[getter]
    fn vid(&self) -> u16 {
        self.vendor_id
    }

    /// Product ID.
    #[getter]
    fn pid(&self) -> u16 {
        self.product_id
    }

    /// Serial number (if available).
    #[getter]
    fn serial(&self) -> Option<String> {
        self.serial_number.clone()
    }

    /// USB bus ID and device address, e.g. `("1-2", 5)`, when known.
    ///
    /// Two probes can share a VID/PID/serial (clones, or two interfaces of one
    /// probe); the location is what tells them apart.
    #[getter]
    fn usb_location(&self) -> Option<(String, u8)> {
        self.usb_location.clone()
    }

    /// Probe identifier name.
    #[getter]
    fn identifier(&self) -> String {
        self.identifier.clone()
    }

    /// Probe type name (e.g. "cmsisdap", "jlink").
    #[getter]
    fn probe_type(&self) -> String {
        self.probe_type.clone()
    }

    /// The probe was found but is not accessible for this user.
    #[getter]
    fn inaccessible(&self) -> bool {
        self.inaccessible
    }

    /// Open this probe.
    ///
    /// Locally-listed probes open a local handle (the direct-probe escape
    /// hatch). Probes listed by the server are opened as a server-side
    /// selector: the returned `Probe` has no local handle, but `attach()`
    /// routes through the server and selects this probe.
    fn open(&self) -> PyResult<ProbePy> {
        if let Some(info) = &self.local {
            let probe = info.open().map_err(to_py_err)?;
            return Ok(ProbePy {
                inner: Some(probe),
                selector: Some(probe_rs::probe::DebugProbeSelector {
                    vendor_id: self.vendor_id,
                    product_id: self.product_id,
                    serial_number: self.serial_number.clone(),
                    interface: self.interface,
                    usb_location: self.usb_location.clone(),
                }),
                is_fake: false,
                speed_khz: None,
                protocol: None,
                enumerated_remotely: false,
            });
        }
        if self.inaccessible {
            return Err(py_err_with_kind(
                format!(
                    "probe {} is not accessible (check permissions, e.g. udev rules)",
                    self.identifier
                ),
                "Probe",
            ));
        }
        Ok(ProbePy {
            inner: None,
            selector: Some(probe_rs::probe::DebugProbeSelector {
                vendor_id: self.vendor_id,
                product_id: self.product_id,
                serial_number: self.serial_number.clone(),
                interface: self.interface,
                usb_location: self.usb_location.clone(),
            }),
            is_fake: false,
            speed_khz: None,
            protocol: None,
            enumerated_remotely: true,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "ProbeInfo(vid={:#06x}, pid={:#06x}, serial={:?}, type={})",
            self.vendor_id, self.product_id, self.serial_number, self.probe_type
        )
    }
}

/// Error for a `Probe` without a local handle: either it was consumed by
/// `attach()`, or it was enumerated by the server and only supports
/// `attach()`/`attach_under_reset()`.
fn no_local_handle(enumerated_remotely: bool) -> PyErr {
    if enumerated_remotely {
        py_err_with_kind(
            "this probe was enumerated by the server and has no local handle; \
             call attach(target) to use it"
                .into(),
            "Probe",
        )
    } else {
        py_err_with_kind("Probe consumed".into(), "Probe")
    }
}

/// An open debug probe.
///
/// This is the direct local-probe escape hatch: probe-level operations
/// (speed, protocol, reset lines) bypass the RPC server. `attach` routes
/// through the server flow, preferring this probe; the local handle is
/// released first so the server can claim the USB device.
#[pyclass(unsendable, name = "Probe")]
pub struct ProbePy {
    pub(crate) inner: Option<Probe>,
    pub(crate) selector: Option<probe_rs::probe::DebugProbeSelector>,
    /// Set when built from a FakeProbe. Such a probe has a local handle only:
    /// the RPC protocol no longer carries a fake-probe attach, so `attach()`
    /// raises instead of claiming a real probe.
    pub(crate) is_fake: bool,
    /// Locally applied probe settings; forwarded into the attach request
    /// because the server owns the real probe handle after attach.
    pub(crate) speed_khz: Option<u32>,
    pub(crate) protocol: Option<WireProtocolProxy>,
    /// True for a probe opened from a server listing: it carries only a
    /// selector, so probe-level operations raise and `attach()` is the way
    /// to use it.
    pub(crate) enumerated_remotely: bool,
}

impl From<Probe> for ProbePy {
    fn from(p: Probe) -> Self {
        Self {
            inner: Some(p),
            selector: None,
            is_fake: false,
            speed_khz: None,
            protocol: None,
            enumerated_remotely: false,
        }
    }
}

#[pymethods]
impl ProbePy {
    /// Attach to a target by name through the probe-rs server, preferring
    /// this probe when it can be identified.
    #[pyo3(signature = (target, permissions=None))]
    pub fn attach(&mut self, target: &str, permissions: Option<Permissions>) -> PyResult<Session> {
        let allow_erase = permissions.unwrap_or_default().allow_erase_all;
        self.attach_via_server(target, allow_erase, false, None)
    }

    /// Attach under reset through the probe-rs server.
    #[pyo3(signature = (target, permissions=None))]
    pub fn attach_under_reset(
        &mut self,
        target: &str,
        permissions: Option<Permissions>,
    ) -> PyResult<Session> {
        let allow_erase = permissions.unwrap_or_default().allow_erase_all;
        self.attach_via_server(target, allow_erase, true, None)
    }

    pub fn set_speed(&mut self, khz: u32) -> PyResult<u32> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.set_speed(khz).map_err(to_py_err)?;
        self.speed_khz = Some(khz);
        Ok(khz)
    }

    pub fn select_protocol(&mut self, protocol: WireProtocolProxy) -> PyResult<()> {
        let wp = match protocol {
            WireProtocolProxy::Swd => WireProtocol::Swd,
            WireProtocolProxy::Jtag => WireProtocol::Jtag,
        };
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.select_protocol(wp).map_err(to_py_err)?;
        self.protocol = Some(protocol);
        Ok(())
    }

    pub fn protocol(&self) -> PyResult<Option<WireProtocolProxy>> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.protocol().map(|p| match p {
            WireProtocol::Swd => WireProtocolProxy::Swd,
            WireProtocol::Jtag => WireProtocolProxy::Jtag,
        }))
    }

    pub fn detach(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.detach().map_err(to_py_err)
    }

    pub fn target_reset(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.target_reset().map_err(to_py_err)
    }

    pub fn target_reset_assert(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.target_reset_assert().map_err(to_py_err)
    }

    pub fn target_reset_deassert(&mut self) -> PyResult<()> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.target_reset_deassert().map_err(to_py_err)
    }

    pub fn get_target_voltage(&mut self) -> PyResult<Option<f32>> {
        let probe = self
            .inner
            .as_mut()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        probe.get_target_voltage().map_err(to_py_err)
    }

    pub fn get_name(&self) -> PyResult<String> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.get_name())
    }

    pub fn speed_khz(&self) -> PyResult<u32> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.speed_khz())
    }

    pub fn has_arm_debug_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.has_arm_debug_interface())
    }

    pub fn has_riscv_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.has_riscv_interface())
    }

    pub fn has_xtensa_interface(&self) -> PyResult<bool> {
        let probe = self
            .inner
            .as_ref()
            .ok_or_else(|| no_local_handle(self.enumerated_remotely))?;
        Ok(probe.has_xtensa_interface())
    }

    /// Construct a Probe from a FakeProbe (consumes the FakeProbe).
    #[staticmethod]
    pub fn from_fake(fp: &mut FakeProbePy) -> PyResult<ProbePy> {
        let inner = fp
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("FakeProbe already consumed"))?;
        Ok(ProbePy {
            inner: Some(inner.into_probe()),
            selector: None,
            is_fake: true,
            speed_khz: None,
            protocol: None,
            enumerated_remotely: false,
        })
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(p) => format!("Probe(name={})", p.get_name()),
            None => "Probe(consumed)".into(),
        }
    }
}

impl ProbePy {
    /// Route attach through the RPC server, preferring this probe's
    /// selector; the local handle is dropped first so the server can claim
    /// the USB device.
    fn attach_via_server(
        &mut self,
        target: &str,
        allow_erase: bool,
        connect_under_reset: bool,
        wait_for_probe: Option<f64>,
    ) -> PyResult<Session> {
        if self.is_fake {
            return Err(crate::error::dry_run_unsupported());
        }
        // Release the local USB claim before the server opens the probe.
        self.inner.take();
        let selector = self
            .selector
            .clone()
            .map(|sel| probe_rs_rpc::probe::DebugProbeSelector {
                vendor_id: sel.vendor_id,
                product_id: sel.product_id,
                interface: sel.interface,
                serial_number: sel.serial_number,
                usb_location: sel.usb_location,
            });
        crate::session::Session::attach_selected(
            target,
            selector,
            self.speed_khz,
            self.protocol,
            allow_erase,
            connect_under_reset,
            wait_for_probe,
        )
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
    #[allow(clippy::wrong_self_convention)] // Python methods take &mut self
    pub fn into_probe(&mut self) -> PyResult<ProbePy> {
        let fp = self
            .inner
            .take()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("FakeProbe already consumed"))?;
        Ok(ProbePy {
            inner: Some(fp.into_probe()),
            selector: None,
            is_fake: true,
            speed_khz: None,
            protocol: None,
            enumerated_remotely: false,
        })
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
