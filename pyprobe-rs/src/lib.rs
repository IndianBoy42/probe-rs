//! pyprobe-rs: Python bindings for the probe-rs embedded debugging toolkit.
//!
//! This crate exposes the probe-rs library's functionality (probe enumeration,
//! target attach, memory/register access, flashing, RTT, trace, and core dumps)
//! as a Python extension module built with PyO3.

mod core_;
mod coredump_;
mod error;
mod events;
mod flashing;
mod open_state;
mod probe_;
mod rpc;
mod rtt_;
mod server;
mod session;
mod types;

use pyo3::prelude::*;

use crate::core_::{CoreHandle, CoreRegisterInfo};
use crate::coredump_::CoreDump;
use crate::flashing::{DownloadOptions, FlashLoader, ProgressEvent, build_loader, download_file};
use crate::probe_::{DebugProbeSelectorPy, FakeProbePy, ListerPy, ProbeInfo, ProbePy};
use crate::rtt_::{ChannelInfo, DownChannel, Rtt, UpChannel};
use crate::session::{Permissions, RegistryPy, Session, SessionConfig, Target};
use crate::types::*;

/// List debug probes. Uses the probe-rs server when remote mode is active or
/// a server is already running; otherwise lists locally without starting one.
#[pyfunction]
fn list_probes(py: Python<'_>) -> PyResult<Vec<ProbeInfo>> {
    if open_state::server_active() {
        let entries = py.detach(|| {
            crate::rpc::rpc_call(
                crate::rpc::DEFAULT_TIMEOUT,
                "probe list",
                open_state::ensure_server_client()?.list_probes(),
            )
        })
        .map_err(error::to_bridge_err)?;
        return Ok(entries.iter().map(ProbeInfo::from_entry).collect());
    }
    Ok(ListerPy::new().list_all())
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

/// Ensure a probe-rs server is running and reachable.
///
/// With no `host`, starts (or adopts) a private local `probe-rs serve` on a
/// user-owned unix socket. With `host`, verifies a remote server
/// (`ws(s)://host:port`, `ssh://[user@]host`, or `socket:///path`).
/// Repeated calls are no-ops; sessions call this implicitly.
#[pyfunction]
#[pyo3(signature = (host=None, token=None))]
fn open(py: Python<'_>, host: Option<String>, token: Option<String>) -> PyResult<()> {
    open_state::set_remote(host, token);
    py.detach(open_state::ensure_server_client)
        .map(|_| ())
        .map_err(error::to_bridge_err)
}

/// Stop the server pyprobe-rs spawned; refused for servers it did not spawn.
#[pyfunction]
fn shutdown_server() -> PyResult<()> {
    open_state::shutdown_server().map_err(error::to_bridge_err)
}

/// Ring buffer of the spawned server's stdout/stderr.
#[pyfunction]
fn server_logs() -> Vec<String> {
    open_state::server_logs()
}

/// Describe the sessions this process currently holds open.
#[pyfunction]
fn sessions() -> Vec<String> {
    open_state::session_list()
}

/// Close every live session created by this process (later calls fail fast).
#[pyfunction]
fn close_all() {
    open_state::close_all()
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
    m.add(
        "ProbeRsError",
        m.py().get_type::<crate::error::ProbeRsError>(),
    )?;

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
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(shutdown_server, m)?)?;
    m.add_function(wrap_pyfunction!(server_logs, m)?)?;
    m.add_function(wrap_pyfunction!(sessions, m)?)?;
    m.add_function(wrap_pyfunction!(close_all, m)?)?;

    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}
