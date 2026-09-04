//! pyprobe-rs: Python bindings for the probe-rs embedded debugging toolkit.
//!
//! This crate exposes the probe-rs library's functionality (probe enumeration,
//! target attach, memory/register access, flashing, RTT, trace, and core dumps)
//! as a Python extension module built with PyO3.

// PyO3 0.29 deprecates the automatic FromPyObject impl for Clone pyclasses.
// We rely on it for passing pyclasses as function arguments (e.g. Permissions).
#![allow(deprecated)]
mod core_;
mod coredump_;
mod error;
mod flashing;
mod probe_;
mod rtt_;
mod session;
mod types;

use pyo3::prelude::*;

use crate::core_::{CoreHandle, CoreRegisterInfo};
use crate::coredump_::CoreDump;
use crate::flashing::{build_loader, download_file, DownloadOptions, FlashLoader, ProgressEvent};
use crate::probe_::{DebugProbeSelectorPy, FakeProbePy, ListerPy, ProbeInfo, ProbePy};
use crate::rtt_::{ChannelInfo, DownChannel, Rtt, UpChannel};
use crate::session::{Permissions, RegistryPy, Session, SessionConfig, Target};
use crate::types::*;

/// List all detected debug probes.
#[pyfunction]
fn list_probes() -> Vec<ProbeInfo> {
    ListerPy::new().list_all()
}

/// List all built-in target names.
#[pyfunction]
fn list_targets() -> Vec<String> {
    let reg = probe_rs::config::Registry::from_builtin_families();
    reg.families()
        .iter()
        .flat_map(|f| f.variants.iter().map(|v| v.name.clone()))
        .collect()
}

/// Look up a target by name from the builtin registry.
#[pyfunction]
fn get_target_by_name(name: &str) -> PyResult<Target> {
    let reg = probe_rs::config::Registry::from_builtin_families();
    reg.get_target_by_name(name)
        .map(|t| Target { inner: t })
        .map_err(error::to_py_err)
}

#[pymodule]
fn _pyprobe_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let _py = m.py();

    // ── Register espressif plugin ──────────────────────────────────
    #[cfg(feature = "espressif")]
    {
        probe_rs_espressif::register_plugin();
    }

    // ── Enums / value proxies ───────────────────────────────────────
    m.add_class::<WireProtocolProxy>()?;
    m.add_class::<ArchitectureProxy>()?;
    m.add_class::<CoreTypeProxy>()?;
    m.add_class::<InstructionSetProxy>()?;
    m.add_class::<EndianProxy>()?;
    m.add_class::<ChannelModeProxy>()?;
    m.add_class::<VectorCatchConditionProxy>()?;
    m.add_class::<CoreStateProxy>()?;
    m.add_class::<HaltKindProxy>()?;
    m.add_class::<TraceSinkProxy>()?;
    m.add_class::<CoreStatusProxy>()?;
    m.add_class::<HaltReasonProxy>()?;
    m.add("ProbeRsError", m.py().get_type::<crate::error::ProbeRsError>())?;

    // ── Probe ────────────────────────────────────────────────────────
    m.add_class::<ListerPy>()?;
    m.add_class::<ProbeInfo>()?;
    m.add_class::<ProbePy>()?;
    m.add_class::<FakeProbePy>()?;
    m.add_class::<DebugProbeSelectorPy>()?;

    // ── Session / Target / Registry ──────────────────────────────────
    m.add_class::<Session>()?;
    m.add_class::<SessionConfig>()?;
    m.add_class::<Permissions>()?;
    m.add_class::<Target>()?;
    m.add_class::<RegistryPy>()?;

    // ── Core ────────────────────────────────────────────────────────
    m.add_class::<CoreHandle>()?;
    m.add_class::<CoreRegisterInfo>()?;

    // ── Flashing ─────────────────────────────────────────────────────
    m.add_class::<DownloadOptions>()?;
    m.add_class::<ProgressEvent>()?;
    m.add_class::<FlashLoader>()?;
    m.add_function(wrap_pyfunction!(download_file, m)?)?;
    m.add_function(wrap_pyfunction!(build_loader, m)?)?;

    // ── RTT ──────────────────────────────────────────────────────────
    m.add_class::<Rtt>()?;
    m.add_class::<UpChannel>()?;
    m.add_class::<DownChannel>()?;
    m.add_class::<ChannelInfo>()?;

    // ── CoreDump ─────────────────────────────────────────────────────
    m.add_class::<CoreDump>()?;

    // ── Module-level convenience functions ───────────────────────────
    m.add_function(wrap_pyfunction!(list_probes, m)?)?;
    m.add_function(wrap_pyfunction!(list_targets, m)?)?;
    m.add_function(wrap_pyfunction!(get_target_by_name, m)?)?;

    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}
